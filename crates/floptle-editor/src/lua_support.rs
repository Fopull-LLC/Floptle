//! Lua authoring support written into every project: the default scripts each
//! project ships with (ADR-0003), and the language-server files (EmmyLua
//! annotations + `.luarc.json`) that give external IDEs hover docs and
//! completion for the engine scripting API.

use std::path::Path;

/// The default Lua scripts every project ships with (ADR-0003): the engine's
/// built-in behaviors, now plain hot-reloadable Lua the user can read and edit.
pub(crate) const DEFAULT_SCRIPTS: &[(&str, &str)] = &[
    // The default new-scene camera's flycam (hold RMB to look, WASD to move) —
    // default_camera_node() attaches `freelook`, so this MUST ship or a fresh
    // project's camera references a script that doesn't exist.
    ("freelook.lua", include_str!("../../../assets/scripts/freelook.lua")),
    ("rotate.lua", include_str!("../../../assets/scripts/rotate.lua")),
    ("pulsate.lua", include_str!("../../../assets/scripts/pulsate.lua")),
    ("float.lua", include_str!("../../../assets/scripts/float.lua")),
    // Ready-made character setups: an FPS body-camera, and a third-person
    // pair (body controller + orbit camera with first-person zoom).
    ("first_person.lua", include_str!("../../../assets/scripts/first_person.lua")),
    ("third_person.lua", include_str!("../../../assets/scripts/third_person.lua")),
    (
        "third_person_camera.lua",
        include_str!("../../../assets/scripts/third_person_camera.lua"),
    ),
    // An arcade fighter on named actions: local versus off one script, with
    // input buffering and motion inputs. The worked example for the whole
    // action layer.
    ("fighter.lua", include_str!("../../../assets/scripts/fighter.lua")),
    // A starting point for strategy games: an isometric camera you pan with
    // WASD or the screen edge, commandable units, and the mouse layer that
    // selects and orders them.
    ("rts_camera.lua", include_str!("../../../assets/scripts/rts_camera.lua")),
    ("rts_unit.lua", include_str!("../../../assets/scripts/rts_unit.lua")),
    ("rts_commander.lua", include_str!("../../../assets/scripts/rts_commander.lua")),
    // Talking to a website: the device-code sign-in flow, written against a
    // configurable `api` param so it points at YOUR server. The worked example
    // for `http.*` and for the one rule that makes an account-backed game
    // possible at all — the server decides what the player owns.
    ("web_login.lua", include_str!("../../../assets/scripts/web_login.lua")),
];

/// EmmyLua type annotations for the engine API, so an external Lua language server
/// (e.g. VSCode's Lua extension) gives hover docs + completion for `node`, `params`,
/// `time`, `dt`, the lifecycle hooks, etc. Written to `.floptle/library/`.
pub(crate) const LUA_ANNOTATIONS: &str = "\
---@meta
--- Floptle engine scripting API (ADR-0003). Generated — do not edit.

---@class Node The node's transform, synced to/from the engine each frame.
---@field x number World X position.
---@field y number World Y position.
---@field z number World Z position.
---@field scale number Uniform scale (shortcut; sets all axes).
---@field scale_x number Scale along X.
---@field scale_y number Scale along Y.
---@field scale_z number Scale along Z.
---@field yaw number Heading about Y, in radians.
---@field pitch number Pitch about X, in radians.
---@field roll number Roll about Z, in radians.
---@field grounded boolean Physics (rigidbody nodes): resting on a surface this frame.
---@field vx number Physics: body velocity X (read/write — set it to drive the body).
---@field vy number Physics: body velocity Y (read/write).
---@field vz number Physics: body velocity Z (read/write).
---@field up_x number Physics: body up (−gravity) X — radial on a planet.
---@field up_y number Physics: body up (−gravity) Y.
---@field up_z number Physics: body up (−gravity) Z.
---@field visible boolean Show / hide this node's geometry (Inspector eye toggle).
---@field persistent boolean Carry this node — and everything under it — across a `scene.load` swap. Its scripts keep RUNNING (`start` does not re-fire), because the node never stopped existing.
---@field pos Vec3 The node's position as a vec3 (read/write: `node.pos = node.pos + dir * dt`). Accepts any {x=,y=,z=} value.
---@field vel Vec3 The body's velocity as a vec3 (read/write) — one write instead of vx/vy/vz: `node.vel = node.vel + node.up * jump`.
---@field up Vec3 The body's up as a vec3 (−gravity): Y on flat ground, RADIAL on a planet. The direction to jump in wherever you're standing.
---@field groundNormal Vec3|nil The floor the body stands on (read-only) — nil while airborne. `groundNormal:dot(node.up)` is the cosine of the slope.
---@field wallNormal Vec3|nil The steepest surface the body is pressed against (read-only), or nil when there's only floor. Stop pushing into it and a walk into a cliff stops launching you into the sky.
---@field forward Vec3 The node's facing as a vec3, from its rotation (−Z forward, like the camera). Works on anything with a transform.
---@field right Vec3 The node's +X axis as a vec3. Pairs with `forward` for camera-relative movement.
---@field size Vec3 The whole scale as a vec3 (read/write). `node.scale` stays the uniform shortcut and also accepts a vec3.
---@field tickX number Physics: the BODY's world X at the start of this tick. Not node.x — that's the INTERPOLATED render pose between ticks, so reading it in fixedUpdate is frame-rate dependent and no rollback replay can reproduce it. Writing this teleports the body without touching the transform.
---@field tickY number Physics: the body's world Y at the start of this tick (read/write).
---@field tickZ number Physics: the body's world Z at the start of this tick (read/write).
---@field tickPos Vec3 Physics: the body's tick pose as a vec3 (read/write). Build hurtboxes from THIS, and move a fighter with `node.tickPos = node.tickPos + vec3(d, 0, 0)` — `node.x = node.x + d` inside fixedUpdate teleports the body onto its VISUAL position, so the model slides and the hitbox doesn't.
---@field layer string Collision/query layer, by project-defined NAME (\"Default\" when unset). Assigning a name the project doesn't define is an ERROR — add layers in Project Settings.
---@field tags string[] The node's tags (a fresh array each read). Assign a whole array to replace; use addTag/removeTag for single edits.
---@field hasTag fun(self: Node, tag: string): boolean Whether the node carries this exact tag.
---@field destroy fun(self: Node) Remove this node and its whole subtree (queued; applied after the pass). Same as `destroy(node)`.
---@field addTag fun(self: Node, tag: string) Add a tag (duplicates are ignored).
---@field removeTag fun(self: Node, tag: string) Remove a tag (no-op when absent).
---@field height number Physics (capsule bodies): standing height - write a smaller value to crouch.
---@field text string|nil UI elements: the label's text (write to change it — numbers coerce, so `hp.text = 42` works).
---@field getcomponent fun(self: Node, name: string): RigidBodyHandle|PointLightHandle|LightHandle|CameraHandle|UiElementHandle|UiSliderHandle|UiLayerHandle|MaterialHandle|nil Live component handle (RigidBody / PointLight / Light / Camera / ParticleSystem / AudioSource / UiElement / UiSlider / UiLayer / Material), nil if the node lacks it.
---@field particles fun(self: Node): ParticleSystemHandle The particle handle for this node's Particle System: play / stop / restart the effect and read its live state.
---@field setShaderParam fun(self: Node, name: string, x: number, y?: number, z?: number, w?: number) Drive a `.flsl` uniform on this node every tick (a GPU uniform write, never a recompile): the node's Material shader, or its UI element's `stage ui` shader (instruments like the navball). Unset lanes are 0.
---@field setCelestial fun(self: Node, t: table) Construction API: set (and create if absent) the node's CelestialBody. Fields (camelCase): mu, bodyRadius, soi, parent (name string), a, e, i, lan, argPe, m0, atmoColor {r,g,b}, atmoHeight, atmoDensity, clouds, luminosity, starColor, occluderRadius (occlusion culling: radius of the solid core geometry never pierces — chunks fully behind it skip their draws; keep it BELOW the deepest cave/dig; 0 = off).
---@field setShaderTexture fun(self: Node, slot: string, ref: string) Point one of this node's `.flsl` shader texture SLOTS somewhere else, at runtime. `slot` is the name the shader declares (`texture ramp` gives \"ramp\"); `ref` is a project-relative image path, an `rt:<name>` render target (what another camera sees, live), or \"\" to clear the slot. A shader can declare up to 8 slots.
---@field setMaterial fun(self: Node, t: table) Construction API — SETUP-TIME, not per-frame: set (and create if absent) the node's Material. It inserts the component and queues a deferred write, so call it on transitions and use `setShaderParam` for values that change every tick. Fields: color/emissive/specular/rim (a colour takes {r,g,b}, {x,y,z}, {1,0.5,0.2} or vec3), emissiveStrength, shininess, specularStrength, rimStrength, unlit (bool), ambient, alpha, texture (path or \"rt:<name>\"), sheetCols/sheetRows/cell (spritesheet grid + which cell draws — `cell` is also a live mirror field, see MaterialHandle).
---@field setTerrain fun(self: Node, id: number) Construction API: make this node a Terrain volume with the given id (generate its field with `terrain.generatePlanet`).
---@field setTerrainGen fun(self: Node, opts: table|nil) Construction API: attach an ON-DEMAND generation spec (same opts table as `terrain.generatePlanet`) — the body's field generates in the background when first approached, so no field file is needed at all (galaxy streaming). Player edits saved under `terrain.saveDir` take priority over regeneration. nil clears the spec.
---@field setPrimitive fun(self: Node, shape: string, color?: table) Construction API: make this node a primitive (\"Cube\"/\"Sphere\"/\"Capsule\"/\"Plane\") with an optional {r,g,b} color.
---@field setCamera fun(self: Node, t: table) Construction API: aim a camera and point it at a RENDER TARGET. Fields: fovY (RADIANS, 0.05–3), active (bool — true clears every other camera's authority), target (a bare name; the picture is the texture \"rt:<name>\", usable on any material or UI image), width/height (the target's pixels, 8–4096), hz (redraws per second, 0 = every frame), cullMask (layer bitmask). A node that is not a camera becomes one. Minimaps, mirrors, security monitors, scopes, split-screen.
---@field sound fun(self: Node): AudioSourceHandle The sound handle for this node's Audio Source: play / stop / pause / swap clips and read playback state.

---A Rigidbody's live tunables (every Inspector field). Assign to change while playing;
---booleans may be written true/false and read back as 1/0.
---@class RigidBodyHandle
---@field friction number Grip, as a coefficient: a ramp holds while tan(its angle) <= friction. 0 is ice, 1 holds exactly 45 degrees, above 1 is grippier still.
---@field slopeLimit number Steepest standable surface, in degrees (default 60). Past it nothing grounds the body and no grip holds it.
---@field restitution number Bounciness 0..1 (0 = no bounce).
---@field gravity number Gravity pull on this body (1/0; assign true/false).
---@field kinematic number Transform-driven mode (1/0; assign true/false, live): never falls or gets pushed, but PUSHES dynamic bodies — platforms, elevators, grabbed objects. (Static mode is the Inspector dropdown — a baked collider, nothing to toggle here.)
---@field shape number Body shape: 0 = sphere, 1 = capsule, 2 = box.
---@field radius number Sphere/capsule radius.
---@field height number Capsule total height.
---@field half_x number Box half-extent X.
---@field half_y number Box half-extent Y.
---@field half_z number Box half-extent Z.
---@field lock_x number Freeze world X translation (1/0).
---@field lock_y number Freeze world Y translation (1/0).
---@field lock_z number Freeze world Z translation (1/0).
---@field lock_rot_x number Freeze rotation about X (1/0).
---@field lock_rot_y number Freeze rotation about Y (1/0).
---@field lock_rot_z number Freeze rotation about Z (1/0).
---@field two_d number 2D: keep the body in the XY plane (1/0).

---A Point Light's live tunables.
---@class PointLightHandle
---@field intensity number Brightness multiplier.
---@field range number Reach in world units.
---@field r number Color red 0..1.
---@field g number Color green 0..1.
---@field b number Color blue 0..1.
---@field shape number The surface it emits from: 0 point, 1 sphere, 2 rect, 3 disk, 4 tube. Assigning keeps the size it had.
---@field width number Rect only: its width in world units (0 on other shapes).
---@field height number Rect only: its height in world units.
---@field radius number Sphere / disk only: its radius in world units.
---@field length number Tube only: how long the bar is.
---@field thickness number Tube only: how thick the bar is.
---@field twoSided number Rect / disk only (1/0): lights out of the back as well as the front.

---The scene's LIGHTING NODE (`find(\"Lighting\"):getcomponent(\"Light\")`) — the one
---environment a world has: the key light, the ambients, the shadows and the fog.
---Conventionally bound to `env`, because `light` is the Point Light handle.
---`ambient2dR/G/B` is where a 2D scene's brightness lives.
---@class LightHandle
---@field ambient2dR number The 2D BASE LIGHT, red 0..1 — the whole light a flat scene has before any 2D light is placed. White by default; turn it down for a dark room a torch can carve a circle out of, and read it back first so you can put it where it was.
---@field ambient2dG number The 2D base light, green 0..1. See ambient2dR.
---@field ambient2dB number The 2D base light, blue 0..1. See ambient2dR.
---@field intensity number Brightness multiplier on the key (directional) light.
---@field colorR number Key light colour red.
---@field colorG number Key light colour green.
---@field colorB number Key light colour blue.
---@field directionX number Key light direction X — lerp the three for a day cycle.
---@field directionY number Key light direction Y.
---@field directionZ number Key light direction Z.
---@field stars number Stars mode (1/0; assign true/false): luminous celestial bodies ARE the key lights.
---@field ambientR number 3D ambient fill red 0..1 — the fill under the key light, deliberately a different value from ambient2dR.
---@field ambientG number 3D ambient fill green 0..1.
---@field ambientB number 3D ambient fill blue 0..1.
---@field shadows number Sun shadows on (1/0; assign true/false). Every shadow field below only applies when this is on.
---@field shadowSoftness number 0 = razor-hard edge … 1 = dreamy-soft penumbra.
---@field shadowStrength number How dark full shadow gets, 0..1 (ambient still fills, so never pitch black).
---@field shadowTintR number Shadows darken toward this colour instead of black — red.
---@field shadowTintG number Shadow tint green.
---@field shadowTintB number Shadow tint blue.
---@field shadowQuantize number 0 = smooth penumbra; 2..8 = posterize it into that many bands (toon/retro).
---@field shadowDither number Bayer-dither the penumbra (1/0) — the classic PS1 dithered shadow edge.
---@field shadowDistance number Max world distance a shadow ray marches before giving up; far geometry stops casting past it.
---@field fog number Depth fog on (1/0; assign true/false).
---@field contactShadows number Contact shadows (1/0): the small dark line where things touch, traced from the depth buffer so a mesh casts its real silhouette. Only what is ON SCREEN casts one.
---@field contactLength number How far a contact shadow traces, in world units.
---@field contactSteps number Samples along the contact trace (2..32).
---@field contactStrength number How dark a contact shadow gets, 0..1.
---@field fogColorR number Fog colour red — match it to the horizon or a seam shows.
---@field fogColorG number Fog colour green.
---@field fogColorB number Fog colour blue.
---@field fogStart number World distance where fog begins (fully clear nearer than this).
---@field fogEnd number World distance where fog is full.
---@field fogDither number Dither the fog gradient to hide 8-bit banding on long ramps (1/0).
---@field fogDitherStrength number Dither amplitude 0..1.
---@field fogVolumetric number Volumetric mode (1/0): march real fog media instead of a distance ramp, so hills poke out of ground mist. fogStart/fogEnd do not apply.
---@field fogDensity number Volumetric: media density per world unit.
---@field fogHeight number Volumetric: world height (y) of the fog layer's top.
---@field fogFalloff number Volumetric: softness of the layer's top edge, world units.
---@field fogNoise number Volumetric: how much drifting noise breaks up the media, 0..1.
---@field fogNoiseScale number Volumetric: noise feature size, world units per repeat.
---@field fogLight number Volumetric: how much of the scene's light scatters in the fog (0 = a flat colour, 1 = lit by the sun/points/bounce, past 1 exaggerates).
---@field fogAnisotropy number Volumetric: which way the media throws light (-0.9..0.9, positive blooms toward the sun).
---@field fogSteps number Volumetric: samples along each pixel's fog ray (2..64).
---@field fogShafts number Volumetric (1/0): march the sun shadow per fog step — the beams, and the cost.

---A Camera's live properties (`node:getcomponent(\"Camera\")`).
---@class CameraHandle
---@field fovY number Vertical field of view, radians.
---@field active number The play-mode view camera (1/0) — assign true to switch to it.

---A UI element's live properties (`node:getcomponent(\"UiElement\")`) — drive a HUD
---from scripts. Position/size numbers follow whatever mode the Inspector set
---(px, %, grow); `text` content is `node.text`.
---@class UiElementHandle
---@field visible number Shown (1/0; assign true/false).
---@field opacity number Multiplies every color the element draws, 0..1.
---@field posX number Free position X / Pin offset X (design units).
---@field posY number Free position Y / Pin offset Y (design units).
---@field width number Width in the axis's sizing mode (px value, % fraction, or grow weight). Absent (nil) on a fit axis; writing one makes it fixed px.
---@field height number Height (same rules as width).
---@field radius number Shape corner radius (design units).
---@field border number Shape border thickness (design units).
---@field fillR number Shape fill red 0..1.
---@field fillG number Shape fill green 0..1.
---@field fillB number Shape fill blue 0..1.
---@field fillA number Shape fill alpha 0..1.
---@field textSize number Text glyph size (design units; ignored while fit is on).
---@field textR number Text color red 0..1.
---@field textG number Text color green 0..1.
---@field textB number Text color blue 0..1.
---@field textA number Text color alpha 0..1.
---@field tintR number Image tint red 0..1.
---@field tintG number Image tint green 0..1.
---@field tintB number Image tint blue 0..1.
---@field tintA number Image tint alpha 0..1.
---@field cell number Spritesheet cell index the image shows (set per frame for sprite animation).
---@field scrollY number Scroll-view position, design units (0 = top; the wheel drives it too, clamped to the content). Present only on elements with the scroll-view option.

---A UI slider's live value (`node:getcomponent(\"UiSlider\")`) — the health-bar hook:
---`bar:getcomponent(\"UiSlider\").value = hp` and the Fill/Handle parts follow.
---@class UiSliderHandle
---@field value number Current value (clamped to min..max at draw time).
---@field min number Range start.
---@field max number Range end.

---A UI layer's live properties (`node:getcomponent(\"UiLayer\")`).
---@class UiLayerHandle
---@field enabled number Master switch (1/0; assign true/false) — an off layer draws nothing.
---@field z number Draw order: lowest z first.
---@field designHeight number Design units that span the window height.
---@field worldSpace number 1 = a panel inside the 3D world at this node's transform; 0 = a screen overlay.
---@field textSnap number Round every rasterized text size to a whole multiple of this many SCREEN PIXELS; 0 = off. For a pixel font whose art is a grid — a cell only looks like a pixel when it lands on a whole one.

---A Material's live SPRITESHEET frame (`node:getcomponent(\"Material\")`) — the
---mesh-side twin of a UI image's cell. Slice the texture into a grid in its
---asset settings, then step the frame every tick:
---`face:getcomponent(\"Material\").cell = math.floor(t * 8) % 16`.
---Everything else about a material goes through `node:setMaterial{...}`.
---@class MaterialHandle
---@field cell number Which cell of the sheet draws (row-major from the top-left; clamped into the grid).
---@field sheetCols number Sheet columns (0 = not a sheet — the whole texture).
---@field sheetRows number Sheet rows.

---A node's tilemap grid, from `node:tilemap()`. Read and write single squares
---to re-dress a room without rebuilding the node.
---
---`cell` is an index into the node's Material spritesheet. To clear a square,
---pass `-1` (any negative works, as in Tiled, Godot and LDtk), `nil`, or the
---`EMPTY_TILE` global — the three are the same value (`floptle/0083`).
---@class TilemapHandle
---@field EMPTY number The cell value meaning \"no tile here\". Same as the EMPTY_TILE global; -1 and nil mean it too.
---@field set fun(self: TilemapHandle, x: number, y: number, cell: number|nil, xform: table|nil) Set one square, 0-based from the TOP-LEFT. Outside the grid is a no-op, not a wrap. A negative or nil cell empties the square. The optional 4th argument turns it: { rot = 0|90|180|270, flipX = bool, flipY = bool }.
---@field get fun(self: TilemapHandle, x: number, y: number): number|nil The cell at (x, y), orientation stripped — nil outside the grid and on an empty square.
---@field at fun(self: TilemapHandle, x: number, y: number): number|nil, number|nil, boolean|nil cell, rot (degrees clockwise), flipX. The whole answer, for art that faces a direction.
---@field fill fun(self: TilemapHandle, cell: number|nil, xform: table|nil) Set every square, including the empty ones. No argument (or -1) clears the whole grid.
---@field fillRect fun(self: TilemapHandle, x0: number, y0: number, x1: number, y1: number, cell: number|nil, xform: table|nil) Fill a rectangle; corners in either order, clipped to the grid.
---@field size fun(self: TilemapHandle): number, number cols, rows.
---@field tileSize fun(self: TilemapHandle): number The world edge length of one square.
---@field resize fun(self: TilemapHandle, opts: table) tm:resize{ cols =, rows =, offsetX =, offsetY = } — keeps whatever overlaps. offsetX/Y is where the old top-left lands, so offsetY = 1 grows a row on top.
---@field cellAt fun(self: TilemapHandle, p: any): number|nil, number|nil Which square a WORLD position (vec3, {x=,y=,z=} or a node) falls in — through the map's own transform, so a moved, turned or scaled map still answers. nil off the map.
---@field worldAt fun(self: TilemapHandle, x: number, y: number): any|nil The world position of that square's CENTRE, or nil off the grid.
---@field tileset fun(self: TilemapHandle): string|nil The project-relative .tileset.ron this map is cut from, or nil.
---@field solid fun(self: TilemapHandle, x: number, y: number): boolean Whether the tileset says that square collides. False with no tileset.
---@field tags fun(self: TilemapHandle, x: number, y: number): table The tileset's tags for that square, as a list. Empty with no tileset.
---@field hasTag fun(self: TilemapHandle, x: number, y: number, tag: string): boolean The common case of tags(), without allocating a table per square.
---@field autotile fun(self: TilemapHandle, x0: number, y0: number, x1: number, y1: number) Recompute the region's autotiled squares (and the one-square ring around it, which is where the stale edges are). Does nothing without a tileset.

---A node's Particle System, controlled from a script via `node:particles()`.
---Start/stop the effect at runtime and read whether it's playing.
---@class ParticleSystemHandle
---@field play fun(self: ParticleSystemHandle) Start emitting if idle (spawns a fresh instance).
---@field stop fun(self: ParticleSystemHandle) Stop + despawn — the live particles vanish.
---@field restart fun(self: ParticleSystemHandle) Re-spawn from t=0 (re-fire a one-shot burst).
---@field isPlaying fun(self: ParticleSystemHandle): boolean Is an instance emitting/ageing right now?
---@field alive fun(self: ParticleSystemHandle): number Live particle count across the effect's tracks.
---@field asset fun(self: ParticleSystemHandle): string|nil The effect asset key this node references.
---@field setIntensity fun(self: ParticleSystemHandle, i: number) Live emission scale 0..4 (1 = authored): multiplies rates/burst counts and shades particle size — drive an engine plume with the throttle.

---A node's Audio Source, controlled from a script via `node:sound()`.
---@class AudioSourceHandle
---@field play fun(self: AudioSourceHandle) Play the source's clip from the start (restarts if already playing).
---@field stop fun(self: AudioSourceHandle) Fade the sound out (a few ms — no click).
---@field pause fun(self: AudioSourceHandle) Freeze playback (resume continues from here).
---@field resume fun(self: AudioSourceHandle) Continue a paused sound.
---@field setClip fun(self: AudioSourceHandle, clip: string) Swap the clip (project-relative path like \"audio/steps.ogg\"); restarts playback if playing.
---@field seek fun(self: AudioSourceHandle, secs: number) Jump the playhead to a time in seconds.
---@field isPlaying fun(self: AudioSourceHandle): boolean Is the source audible right now?
---@field position fun(self: AudioSourceHandle): number Playhead in seconds.

---Something that walks the navmesh, returned by `nav.agent(node)`. Order it with
---`moveTo` and read `state` as it goes — the engine steps the whole crowd once a
---frame, so there is no update to call.
---@class NavAgentHandle
---@field state string 'idle' | 'moving' | 'arrived' | 'blocked' | 'crossing', and 'gone' once destroyed.
---@field arrived boolean True once it got there — the flag to hang \"and then attack\" off.
---@field moving boolean True while it still has somewhere to be.
---@field blocked boolean True when it cannot get there right now: unreachable, or no progress for giveUpAfter seconds. A crowd pin clears itself; a cut-off goal does not.
---@field offMesh boolean True when the order named a place the navmesh does not cover, rather than one it cannot reach.
---@field complete boolean Whether the route it is walking actually reaches the order.
---@field remaining number How far there is left to walk, ALONG THE ROUTE rather than through the walls.
---@field velocity Vec3 How fast it is going. With drive = 'none' this is the whole output.
---@field speed number Ground speed in units per second — what a walk/run blend reads.
---@field pos Vec3 Where it is, in world space.
---@field target Vec3|nil Where it was told to go, or nil with no order.
---@field link string|nil The Nav Link being crossed right now, by name. The hook for a climb animation.
---@field linkProgress number|nil How far across that link, 0 to 1.
---@field alive boolean False once the agent (or its node) has gone.
---@field moveTo fun(self: NavAgentHandle, point: any) Send it to a world point. Idempotent, so calling it every frame to chase a moving target is fine.
---@field stop fun(self: NavAgentHandle) Cancel the order. Anything mid-crossing finishes crossing first.
---@field teleport fun(self: NavAgentHandle, point: any) Put it somewhere without walking there.
---@field set fun(self: NavAgentHandle, opts: table) Change how it walks mid-game; anything left out is left alone.
---@field corners fun(self: NavAgentHandle): table The corners still to walk, as a list of vec3.
---@field destroy fun(self: NavAgentHandle) Take it out of the crowd.

---A playing sound returned by `audio.play(...)`. Handles stay valid until the
---sound finishes; calls on a finished sound are ignored.
---@class SoundHandle
---@field stop fun(self: SoundHandle) Fade the sound out and end it.
---@field pause fun(self: SoundHandle) Freeze playback.
---@field resume fun(self: SoundHandle) Continue a paused sound.
---@field setVolume fun(self: SoundHandle, volume: number) Linear volume (1 = as authored).
---@field setPitch fun(self: SoundHandle, pitch: number) Playback-rate pitch (0.5 = octave down, 2 = octave up).
---@field setPan fun(self: SoundHandle, pan: number) Stereo pan −1..1 (non-spatial sounds).
---@field setTrack fun(self: SoundHandle, track: string) Re-route through a mixer track (\"Master\" or a track name).
---@field setPosition fun(self: SoundHandle, x: number, y: number, z: number) Move the emitter (stops following a node).
---@field seek fun(self: SoundHandle, secs: number) Jump the playhead to a time in seconds.
---@field isPlaying fun(self: SoundHandle): boolean Still audible (false once finished)?
---@field position fun(self: SoundHandle): number Playhead in seconds.

---A mixer track handle from `audio.track(name)` — live control of the
---project mixer (reverts to the saved mixer when Play stops).
---@class AudioTrackHandle
---@field setVolume fun(self: AudioTrackHandle, db: number) Fader gain in dB (0 = unity, −60 = silent).
---@field setPan fun(self: AudioTrackHandle, pan: number) Stereo pan −1..1.
---@field setMuted fun(self: AudioTrackHandle, muted: boolean) Mute / unmute the track.
---@field setSoloed fun(self: AudioTrackHandle, soloed: boolean) Solo the track (mutes everything else).

---Options for `audio.play`. All fields optional.
---@class AudioPlayOpts
---@field volume number? Linear volume (default 1).
---@field pitch number? Playback rate (default 1; also shifts pitch).
---@field pan number? Stereo pan −1..1 (non-spatial sounds).
---@field mode string? \"Spatial\" (default) | \"Distance\" (no panning) | \"Flat\" (2D).
---@field falloff string? \"Inverse\" (default) | \"Linear\" | \"Exponential\".
---@field minDistance number? Full volume inside this range (default 2).
---@field maxDistance number? Silent past this range (default 50).
---@field track string? Mixer track to route through (default Master).
---@field endBehavior string? \"Stop\" (default) | \"Destroy\" (despawn the followed node) | \"Loop\".
---@field loop boolean? Shorthand for endBehavior = \"Loop\".

---The sound system: fire-and-forget playback + mixer control. Positions and
---following make it spatial; pass no position for flat 2D (UI, music).
---@class audio
---@field play fun(clip: string, a?: Node|number, b?: number|AudioPlayOpts, c?: number, opts?: AudioPlayOpts): SoundHandle Play a clip: `audio.play(\"audio/ding.ogg\")` (flat) · `audio.play(\"audio/hit.ogg\", x, y, z, opts)` (at a point) · `audio.play(\"audio/engine.ogg\", carNode, {loop=true})` (follows the node).
---@field stopAll fun() Stop every playing sound (sources and one-shots).
---@field track fun(name: string): AudioTrackHandle A mixer track handle (\"Master\" or a track name from the Mixer tab).
audio = {}

---This instance's tunables, seeded from the script's `defaults` table.
---@type table<string, number>
params = {}

---Seconds since play started.
---@type number
time = 0.0

---Seconds since the last frame (also passed to update).
---@type number
dt = 0.0

---The tunables this script declares (shown in the Inspector).
---@type table<string, number>
defaults = {}

---Print a message to the engine console.
---@param msg string
function log(msg) end

---Spawn a one-shot particle effect at a world point — no node required. It plays
---once and despawns itself. Great for hits, pickups, footstep poofs.
---e.g. `spawnEffect(\"vfx/Explosion\", hit.x, hit.y, hit.z)`
---@param key string Effect asset key (project-relative, no `.vfx.ron`).
---@param x number
---@param y number
---@param z number
function spawnEffect(key, x, y, z) end

---The runtime 3D line layer. Immediate mode: a segment lives ONE frame — call
---it every `lateUpdate` (preferred: it runs in the camera pass, so lines land
---the same frame as the camera that framed them) or `update`/`fixedUpdate`
---while you want the line on screen. Drawn OVER the scene (never occluded —
---KSP-style orbit lines read through planets). This is what the map draws its
---conics with.
draw = {}

---Queue one world-space line segment for this frame.
---e.g. `draw.line(a.x, a.y, a.z, b.x, b.y, b.z, 0.3, 0.85, 1.0)`
---@param x1 number @param y1 number @param z1 number
---@param x2 number @param y2 number @param z2 number
---@param r number 0..1 @param g number 0..1 @param b number 0..1
---@param a? number alpha, default 1
function draw.line(x1, y1, z1, x2, y2, z2, r, g, b, a) end

---Spawn a PREFAB instance (make one by dragging a node into the Assets panel).
---`\"bullet\"` finds `prefabs/bullet.prefab.ron`; subfolder names and full
---paths work too. `pos` places the first root (a vec3/table/node); `fn(root)`
---runs with the new node's handle the same frame:
---`spawn(\"bullet\", node.pos + dir, function(b) b.vx = dir.x * 40 end)`
---Local-only in multiplayer — the server uses `net.spawn` for replicated objects.
---@param prefab string Prefab name or path.
---@param pos? Vec3 World position for the first root.
---@param fn? fun(root: Node) Configure the freshly spawned root.
function spawn(prefab, pos, fn) end

---Remove a node AND its whole subtree (physics body included). Queued —
---applied after the pass, so the handle stays readable through this call.
---Method form: `node:destroy()`. On a client, replicated nodes refuse
---(server authority — use `net.despawn` on the server).
---@param target Node The node (or node handle) to remove.
function destroy(target) end

---Create a PLAIN node (Empty matter, identity transform) — the construction
---hook for script-built content (procgen, editor actions). The callback gets
---the new node's handle: combine with setTerrain / setCelestial /
---setPrimitive / setMaterial + transform writes to build anything.
---`createNode(\"Oria\", function(n) n:setTerrain(2); n.x = 500 end)`
---@param name string
---@param parent? Node Parent node (nested creates inside callbacks are fine).
---@param fn? fun(n: Node) Configure the freshly created node.
function createNode(name, parent, fn) end

---Runs once when play begins (optional).
---@param node Node
function start(node) end

---Runs every frame while playing.
---@param node Node
---@param dt number Seconds since the last frame.
function update(node, dt) end

---Runs every GAMEPLAY TICK (60 Hz, constant dt) — put movement, gameplay, and
---physics writes here; keep cameras/cosmetics in `update`. This is the fixed,
---deterministic cadence physics steps at (and the one multiplayer prediction
---will replay), so tick code behaves the same at any frame rate.
---@param node Node
---@param dt number The constant tick delta (1/60 s by default).
function fixedUpdate(node, dt) end

---Runs once per frame AFTER physics and the interpolated transform writeback —
---the CAMERA pass. Anything that follows something else (orbit cameras, name
---tags, listeners) belongs here so it samples this frame's FINAL poses;
---following from `update` reads last frame's pose (a velocity × dt lag that
---turns frame-time noise into visible jitter).
---@param node Node
---@param dt number Seconds since the last frame.
function lateUpdate(node, dt) end

---Mark a `defaults` entry as a NODE REFERENCE: `defaults = { hpBar = noderef() }`
---shows a node picker in the Inspector (or drag a node from the Hierarchy onto
---the slot), and the script reads `params.hpBar` as a node handle (or nil while
---unwired) — no `find()` needed.
---@return any
function noderef() end

---Mark a `defaults` entry as a SCRIPT REFERENCE: `defaults = { hp = scriptref(\"health\") }`
---binds to that script ON the wired node — `params.hp` is a script handle directly
---(call its functions, read its state). The Inspector lists only nodes carrying it.
---@param kind string The script name (its .lua file stem).
---@return any
function scriptref(kind) end

---Mark a `defaults` entry as a COMPONENT REFERENCE: `defaults = { body = componentref(\"RigidBody\") }`
---binds to that component ON the wired node — `params.body.friction = 0.05` directly.
---Components: RigidBody, PointLight, Camera, ParticleSystem, UiElement, UiSlider, UiLayer.
---@param name string The component name.
---@return any
function componentref(name) end

---UI button hook: fires when this node's UI element (with `button` on) is clicked
---(pressed AND released on it). Also available: `pressed`, `released`,
---`hoverStart`, `hoverEnd` — same signature. Style the states here (no imposed look).
---@param node Node
function clicked(node) end

---UI button hook: the pointer entered this node's element. Pair with `hoverEnd`.
---@param node Node
function hoverStart(node) end

---UI hook: keyboard/gamepad focus arrived here. What focus LOOKS like is your
---style's `focus` block; this is for the rest — a sound, a preview, a
---description panel. Pair with `focusExit`.
---@param node Node
function focusEnter(node) end

---UI hook: the `UiCancel` action (Escape / B) while this element has focus.
---@param node Node
function cancelled(node) end

---UI text-field hook: Enter in a focused field. Read the value with `node.text`.
---A field fires this INSTEAD of `clicked`, so a field inside a button doesn't
---also run the button.
---@param node Node
function submitted(node) end

---UI text-field hook: the value changed (typing, paste, backspace). Once per
---frame however many keystrokes landed.
---@param node Node
function changed(node) end

---UI hook: a `draggable` element was picked up. The engine does NOT move it and
---draws no ghost — a card that tilts and an item that snaps to a grid are both
---drags, so the look is yours. Also: `dragMove`, `dragCancel`, `dropped`, and on
---the target `dragEnter` / `dragOver` / `dragLeave` / `dropped`.
---@param node Node
function dragStart(node) end

---UI hook: a completed drag. Fires on BOTH ends — the target that now has it and
---the source that gave it away. `ui.dragging()` / `ui.dropTarget()` name the pair.
---@param node Node
function dropped(node) end

---The game-UI runtime: focus and drags. Everything else about an element is a
---component (`node:getcomponent`); these two are engine state, because a focus
---ring that survived into a saved scene would be a bug.
---@class Ui
ui = {}
---Move the keyboard/gamepad focus. `ui.focus(nil)` drops it. Focusing a text
---field starts editing it.
---@param node Node|nil
function ui.focus(node) end
---The focused element, or nil — or, given an element, whether it is the focused
---one. Also readable per-node as `node.focused`.
---@param element Node|nil
---@return Node|boolean|nil
function ui.focused(element) end
---Listen to an element from a script that does NOT live on it.
---
---A `clicked` function answers for the node its script is on, so a menu of eight
---buttons wants eight script files — each three lines long, each really saying
---\"tell the menu\". This puts all eight in the menu's own script, where the
---state they change already lives.
---
---The handler is called `fn(element, hook)`, so one function can serve a whole
---row of buttons. Registering again for the same element and hook REPLACES,
---which makes calling it from `update` harmless. A listener dies with its
---element or with the script that registered it, and a hot reload re-registers.
---@param element Node
---@param hook string clicked|pressed|released|hoverStart|hoverEnd|changed|submitted|cancelled|focusEnter|focusExit|dragStart|dragMove|dragEnter|dragOver|dragLeave|dragCancel|dropped
---@param fn fun(element: Node, hook: string)
function ui.on(element, hook, fn) end
---Stop listening: every hook this script has on the element, or just one.
---Only this script's own — two managers on one button must not be able to
---unregister each other.
---@param element Node
---@param hook string|nil
function ui.off(element, hook) end
---Did this element fire `clicked` this frame? The polling half of `ui.on`, for
---a manager that already has an `update`. Both read the same event list, so a
---poll and a hook can never disagree about what happened.
---@param element Node
---@return boolean
function ui.clicked(element) end
---Did LMB go down on this element this frame?
---@param element Node
---@return boolean
function ui.pressed(element) end
---Did LMB come back up this frame (on or off the element)?
---@param element Node
---@return boolean
function ui.released(element) end
---Did this text field's value change this frame?
---@param element Node
---@return boolean
function ui.changed(element) end
---Was Enter pressed in this focused text field this frame?
---@param element Node
---@return boolean
function ui.submitted(element) end
---Did this element fire that hook this frame? Any hook by name.
---@param element Node
---@param hook string
---@return boolean
function ui.event(element, hook) end
---Everything that happened on the UI this frame, as `{ node = , event = }`
---rows, optionally filtered to one hook. Lets one manager handle a whole screen
---without naming a single element.
---@param hook string|nil
---@return table[]
function ui.events(hook) end
---The element under the pointer, or nil — or, given an element, whether the
---pointer is over it. A STATE, not an event: true for as long as it is true
---(`hoverStart` / `hoverEnd` are the edges).
---@param element Node|nil
---@return Node|boolean|nil
function ui.hovered(element) end
---The element the pointer is holding down, or nil — or, given an element,
---whether it is being held. Hold-to-charge, press-and-hold repeat, a dip while
---pressed.
---@param element Node|nil
---@return Node|boolean|nil
function ui.held(element) end
---The element being dragged, or nil — live for the whole drag and for the frame
---the `dropped` hooks run on. There is no separate payload channel: a node
---already carries params, a name and tags, so ask it what it is.
---@return Node|nil
function ui.dragging() end
---The drop target the drag is currently over, or nil.
---@return Node|nil
function ui.dropTarget() end
---Say a relationship once instead of writing an `update` that keeps it true:
---the engine calls `fn` once a frame, after every update, and writes what it
---returns. A string or number goes to `text`; a `color(...)` to a colour field;
---a number or boolean to whichever component actually has that field (so
---`\"value\"` finds the slider). Re-binding the same property replaces it.
---@param node Node
---@param property string
---@param fn fun(): any
function ui.bind(node, property, fn) end
---Drop every binding on a node, or just one property's.
---@param node Node
---@param property string|nil
function ui.unbind(node, property) end
---Build a UI subtree from data and reconcile it with the one already there.
---
---An element is `{ \"kind\", prop = value, ..., children }`, where kind is one of
---box / row / col / text / image / button / field / slider / scroll. Give a
---container `items = {...}` and a function child to get one child per item —
---the function receives `(item, i)` and may return nil to skip. `key = \"id\"`
---is how a row keeps its entity through a re-sort, and `onClicked = function(node) end`
---(any UI hook, `on` + its name) carries behaviour inline.
---
---Call it again when the data changes: only the difference is spawned and
---destroyed, so the rows that stay keep their hover, their scroll, their
---transitions and what was typed into them. A property the table stops
---mentioning goes back to default. Play only; a mistyped property raises.
---@param container Node
---@param tree table
function ui.make(container, tree) end

---A colour: `color(r, g, b [, a])`, `color(gray [, a])`, or `color(other, a)`
---to copy with a new alpha. Channels are 0..1 and alpha defaults to 1, so
---`color(1, 0, 0)` is opaque red. It is a plain `{r, g, b, a}` table (also
---indexable `[1]`..`[4]`), so it prints, saves and compares — and any
---`{1, 0, 0}` your project already had is already a colour.
---
---Assign one whole: `el.fill = color(1, 0.85, 0.35)`. Also `textColor`,
---`borderColor`, `tint`, `groupTint`, `caretColor`, `selectionColor`,
---`placeholderColor`.
---@overload fun(gray: number, a: number|nil): table
---@overload fun(other: table, a: number|nil): table
---@param r number
---@param g number
---@param b number
---@param a number|nil
---@return table
function color(r, g, b, a) end
---`color.hex(\"#ff8800\")` — 6 or 8 hex digits. A 3-digit shorthand is refused
---rather than guessed at.
---@param s string
---@return table
function color.hex(s) end
---Blend two colours per channel; `t` is clamped to 0..1.
---@param a table
---@param b table
---@param t number
---@return table
function color.lerp(a, b, t) end

---Multiplayer (docs/netcode-design.md). Mark nodes with the Networked component,
---declare synced vars with a top-level `replicated = { hp = 100 }` table (read/
---write them as `synced.hp` — the server owns them), handle remote calls with
---`onRpc = {}` + `function onRpc.name(args, sender) end`.
---@class Net
net = {}
---Become the authoritative host. `relay = \"addr\"` hosts through a
---rendezvous relay: you get a LOBBY CODE, friends join with it, nobody
---port-forwards. `port = n` hosts directly on UDP (QUIC) for LAN/self-host.
---Neither: the in-editor loopback harness.
---@param opts { maxPlayers: integer, port: integer, relay: string, interest: number, interestBudget: integer, inputDelay: integer }|nil
function net.host(opts) end
---Set the rollback input delay in TICKS (clamped to 6), for the NEXT match.
---
---Rollback holds your own input a few ticks so the opponent's has time to
---arrive. Too low and theirs lands after the tick that needed it, on every
---tick, so the driver guesses and re-simulates — correct, and five times the
---work. Omit it and the host derives one from the worst peer's measured RTT
---(2 on a LAN, 5 across a country).
---
---Fixed for a session on purpose: adaptive delay hides a bad connection by
---changing how the game feels while you are playing it. Call this between
---matches — the roster re-announce restarts the driver on a fresh origin.
---@param ticks integer
function net.setInputDelay(ticks) end
---Join a session: `\"relay://relayaddr/CODE\"` = a lobby code through a
---relay (no port-forwarding), `\"quic://host:port\"` = a server directly,
---`\"local://\"` = the in-editor test harness.
---@param addr string
function net.join(addr) end
---Leave / end the session.
function net.leave() end
---This endpoint's role.
---@return \"offline\"|\"server\"|\"client\"
function net.role() end
---@return boolean
function net.isServer() end
---@return boolean
function net.isClient() end
---Connected client peer ids (server).
---@return integer[]
function net.peers() end
---Round-trip time in milliseconds.
---@param peer integer|nil
---@return number
function net.ping(peer) end
---How a join attempt is going: \"offline\", \"connecting\", \"joined\" or
---\"refused\". Second return is WHY, on \"refused\" — the relay's own words,
---e.g. \"no lobby QK7RM\".
---
---Wait on this, not on net.role(): joining does not block, so role reads
---\"client\" from the frame you called net.join, whether or not that code
---matched any lobby.
---@return string state
---@return string|nil reason
function net.joinState() end
---The lobby code friends type in to join, on a host that used
---`net.host{ relay = ... }`. Put it on your own lobby screen.
---
---nil until the relay answers (poll it, don't read it once), and nil for good
---on a client or a direct/LAN host — there is no code there, joiners use the
---address.
---@return string|nil
function net.lobbyCode() end
---Send a named remote call. On the server it goes to clients (all, or `to`);
---on a client it goes to the server. Args: scalars + tables (≤4 deep, ≤1KB).
---Handle with `function onRpc.name(args, sender) end`.
---`withInput = true` (client → server) stamps the call with the tick you were
---SEEING when you fired — the server can then judge it with `net.rewind`.
---@param name string
---@param args any|nil
---@param opts { to: integer, withInput: boolean }|nil
function net.rpc(name, args, opts) end
---SERVER ONLY, inside an `onRpc` handler for an rpc sent `{withInput = true}`:
---run `fn` against the world as `peer` PERCEIVED it — raycasts see every
---networked body where that player saw it (their interp-delayed view), and
---other scripts' `synced` vars read the values from that same tick. A parry
---that was up on the attacker's screen counts. Restores the present after
---`fn`; returns whatever `fn` returns. Rewind depth is clamped to ~250 ms.
---@param peer integer The rpc's sender.
---@param fn function
---@return any ...
function net.rewind(peer, fn) end
---Listen for session events: \"playerJoined\"|\"playerLeft\" (fn gets the peer id),
---\"connected\", \"disconnected\" (fn gets a reason string).
---@param event string
---@param fn function
function net.on(event, fn) end
---SERVER ONLY: spawn a scene asset's first node as a replicated runtime object.
---It appears on every client (and late joiners). Available next tick.
---@param path string Scene asset, project-relative (e.g. \"scenes/arrow.ron\").
---@param opts { x: number, y: number, z: number, owner: integer }|nil
function net.spawn(path, opts) end
---SERVER ONLY: despawn a replicated runtime object everywhere.
---@param node Node
function net.despawn(node) end
---Deterministic RNG for a rollback session: drawn from (match seed, tick, draw
---index), so every peer rolls the same numbers AND a re-simulated tick rolls
---them again. Use this instead of `rng()` in anything a rollback node reads —
---an unseeded roll comes from the clock, and two peers drawing differently is
---a match that quietly forks in two.
---@param a number|nil Omitted → [0,1). One arg → an integer 1..a. Two → a..b.
---@param b number|nil
---@return number
function net.random(a, b) end
---True while the engine is RE-SIMULATING ticks it already ran after a rollback
---correction. For cosmetics the engine can't gate for you (a material poke, a
---UI label). NEVER branch simulation on it: a replayed tick that computes
---something different from the live tick is the definition of a desync.
---@return boolean
function net.replaying() end
---Ticks re-simulated by the most recent rollback correction.
---@return number
function net.rollbackDepth() end
---The deepest rollback this session has had to perform.
---@return number
function net.rollbackMax() end
---Mean ticks re-simulated per correction — the texture of the connection,
---where rollbackMax is only its worst moment.
---@return number
function net.rollbackAverage() end
---0..1 — the fraction of simulated ticks that had to guess a peer's input.
---@return number
function net.mispredictRate() end
---The session's FIXED input delay, in ticks. Never changes mid-match.
---@return number
function net.inputDelay() end
---True while the sim is waiting for input rather than guessing past the depth
---cap — the game runs slightly slow instead of teleporting the opponent. Show
---your own \"connection trouble\" banner off this.
---@return boolean
function net.stalled() end

---Is this node under MY control on this machine? Offline / non-networked →
---true. Server → true unless a remote peer owns it. Client → true only for
---your own predicted node(s). THE way for shared scripts (cameras, HUDs) to
---pick the local player out of many identical avatars:
---`for _, s in ipairs(findScripts(\"third_person\")) do if net.isMine(s.node) then ... end end`
---@param node Node
---@return boolean
function net.isMine(node) end

---Per-script synced variables: declare `replicated = { hp = 100 }` at the top
---level, then read/write `synced.hp`. The SERVER's writes replicate to every
---client; client writes warn (the server will overwrite them).
---@type table<string, any>
synced = {}

---Player input (play mode) — poll the keyboard + mouse to make games interactive.
---@class Input
input = {}
---True while `name` is held. Names: a-z, 0-9, space, enter, shift, ctrl, alt, left/right/up/down, escape, tab.
---@param name string
---@return boolean
function input.key(name) end
---True only on the frame `name` goes down (a key-press edge).
---@param name string
---@return boolean
function input.pressed(name) end
---The CHARACTERS entered this frame, as a string, resolved by the OS keyboard
---layout — with a paste (Ctrl/Cmd-V) folded in.
---
---A different question from `input.pressed`, which is PHYSICAL: `\"q\"` is the key
---where Q sits on a QWERTY board and types `a` on AZERTY. Building a string by
---polling keys gets the alphabet wrong for anyone whose keyboard isn't yours.
---Never contains control characters — Enter and Backspace stay actions. Empty
---while a UI text field has focus, because the field consumed them.
---@return string
function input.typed() end
---The ACTIVE camera's world yaw (radians), captured with the input snapshot.
---THE way to do camera-relative movement in multiplayer: the aim rides the
---input command, so the server and prediction replay use exactly the angle
---the player saw. nil when the scene has no active camera.
---@return number|nil
function input.aimYaw() end
---The active camera's world pitch (radians), captured with the input snapshot.
---@return number|nil
function input.aimPitch() end
---A -1/0/1 axis from a negative/positive key pair, e.g. input.axis(\"a\", \"d\").
---@param neg string
---@param pos string
---@return number
function input.axis(neg, pos) end
---The cursor position in pixels: `local x, y = input.mouse()`.
---@return number, number
function input.mouse() end
---Mouse movement since last frame: `local dx, dy = input.mouse_delta()`.
---@return number, number
function input.mouse_delta() end
---Mouse wheel delta this frame.
---@return number
function input.scroll() end
---True while a mouse button is held (0 left, 1 right, 2 middle).
---@param i integer
---@return boolean
function input.button(i) end
---True only on the frame a mouse button goes down.
---@param i integer
---@return boolean
function input.clicked(i) end

--- ACTIONS ---------------------------------------------------------------
--- Named actions from Project Settings → Input. Prefer these over the raw
--- polls above: they work on a gamepad, the player can rebind them, and they
--- are what multiplayer replicates (raw polls read NEUTRAL on a Predicted
--- node, because the wire carries actions).

---True while the named action is held — any of its bindings (a key, a mouse
---button, a pad button, a trigger past its threshold).
---@param name string
---@return boolean
function input.action(name) end
---True only on the frame/tick the action goes down.
---@param name string
---@return boolean
function input.justPressed(name) end
---True only on the frame/tick the action goes up.
---@param name string
---@return boolean
function input.justReleased(name) end
---How long the action has been continuously held, in seconds. 0 when up.
---Use it for hold-to-charge without tracking your own timer.
---@param name string
---@return number
function input.heldSecs(name) end
---A named 1D axis in -1..1 (triggers, the wheel, a key pair).
---@param name string
---@return number
function input.axis1(name) end
---A named 2D axis, clamped to the unit disk: `local x, y = input.axis2(\"Move\")`.
---Reads identically whether the player is on WASD or a stick.
---@param name string
---@return number, number
function input.axis2(name) end

--- FIGHTER LAYER (fixedUpdate only) --------------------------------------
--- These read the input HISTORY, which advances once per gameplay tick. Call
--- them from `fixedUpdate`; from `update` they answer about the last tick.

---The current numpad direction from the \"Move\" axis, written from the
---character's point of view (see input.setFacing):
---  7 8 9      up-back    up     up-forward
---  4 5 6  =   back     neutral    forward
---  1 2 3      dn-back   down   dn-forward
---@return integer
function input.dir() end
---How many consecutive ticks a numpad direction has been held — build your own
---charge or leniency rules on top of it.
---@param dir integer
---@return integer
function input.dirHeldTicks(dir) end
---Was the action pressed within the last `ticks` ticks and not yet consumed?
---This is the input buffer: a player who hits Punch two frames before their
---recovery ends still gets the punch. `ticks` defaults to 3.
---@param name string
---@param ticks integer|nil
---@return boolean
function input.buffered(name, ticks) end
---Spend a buffered press so it fires exactly once. Without this a 4-tick
---buffer fires your attack on all four ticks.
---@param name string
---@param ticks integer|nil
---@return boolean
function input.consume(name, ticks) end
---Has the named motion been completed recently? Motions are defined in
---input.ron; the standard set ships seeded: qcf, qcb, dp, rdp, hcf, hcb, dd,
---ff, bb, chargeF, chargeU. `window` overrides the map's tick window.
---@param name string
---@param window integer|nil
---@return boolean
function input.motion(name, window) end
---Which way this player faces: +1 normal, -1 mirrored. Directions are flipped
---before they reach the history, so `motion(\"qcf\")` keeps meaning \"toward the
---opponent\" after a cross-up. The engine has no opinion about who faces where.
---@param facing number
function input.setFacing(facing) end
---@return number
function input.facing() end

--- LOCAL MULTIPLAYER ------------------------------------------------------

---The same input API bound to another local player (1-based). Two characters
---can run the SAME script: pass the slot in as a script param —
---  local me = input.player(params.player)
---  if me.justPressed(\"Punch\") then ... end
---@param n integer
---@return Input
function input.player(n) end

--- CONTEXTS + REBINDING (settings menus) ----------------------------------

---Push a named input layer. A `consume` layer swallows every action it doesn't
---list, so a menu can eat movement without the player controller knowing:
---  input.pushContext(\"menu\", { priority = 100, consume = true, enabled = { \"Pause\" } })
---@param name string
---@param opts table|nil
function input.pushContext(name, opts) end
---Pop a layer by name. Returns whether one was removed.
---@param name string
---@return boolean
function input.popContext(name) end
---Every action name in the map — for drawing a settings screen.
---@return string[]
function input.actions() end
---An action's bindings as printable chips (e.g. \"⌨ Space\", \"🎮 South\").
---@param name string
---@return string[]
function input.bindingsOf(name) end
---Arm press-to-bind for an action. `filter` is \"keyboard\", \"pad\", \"axis\", or
---nil for any button. Escape always cancels rather than binding.
---@param name string
---@param filter string|nil
function input.startRebind(name, filter) end
---The captured binding's chip once something was pressed, \"\" while waiting,
---or nil when nothing is armed.
---@return string|nil
function input.pendingRebind() end
---Apply the captured binding. Returns false if nothing was captured, or if
---that binding already existed.
---@return boolean
function input.commitRebind() end
function input.cancelRebind() end

---The active game camera's projection — turn a world point into a screen pixel
---(and back). The pixels are in the SAME space `input.mouse()` reports, so you
---can hover/click 3D things you drew: sample a line into points, project each,
---and keep the nearest to the cursor (that's how the map's click-on-orbit works).
---@class Camera
camera = {}
---True once the editor is feeding a live game camera (false in the Scene view).
---@return boolean
function camera.exists() end
---The game viewport size in pixels: `local w, h = camera.screenSize()`.
---@return number, number
function camera.screenSize() end
---Project a world point to the game view: `sx, sy, depth, onscreen`. `onscreen`
---is false for points behind the camera or outside the frustum — skip those.
---@param x number
---@param y number
---@param z number
---@return number, number, number, boolean
function camera.worldToScreen(x, y, z) end
---A world-space ray from a screen pixel: `ox,oy,oz, dx,dy,dz` (origin on the near
---plane, unit direction into the scene). The inverse of `worldToScreen`.
---@param sx number
---@param sy number
---@return number, number, number, number, number, number
function camera.screenToRay(sx, sy) end

---Cast a ray against the world's colliders (terrain + meshes + primitives)
---AND every physics body (players, crates). Returns a hit table
---{x,y,z, nx,ny,nz, distance, node} or nil — `node` is the hit body's node
---handle (nil for static geometry), so `hit.node:getscript(\"combat\")` works.
---Your OWN node's body is excluded (a ray from your center never hits you);
---pass another node as `ignore` to skip its body too (e.g. an orbit camera
---ignoring the character it follows). The last arg can instead be an OPTIONS
---table: `raycast(x,y,z, dx,dy,dz, max, { ignore = target, layers = {\"Ground\"} })`
---— `layers` (a name or an array of names, Project Settings → Layers) filters
---BOTH static geometry and bodies; a misspelled layer name is an error.
---@param ox number
---@param oy number
---@param oz number
---@param dx number
---@param dy number
---@param dz number
---@param max number
---@param ignore Node|{ ignore: Node|nil, layers: string|string[]|nil }|nil A node whose body the ray passes through, or an options table.
---@return { x: number, y: number, z: number, nx: number, ny: number, nz: number, distance: number, node: Node|nil }|nil
function raycast(ox, oy, oz, dx, dy, dz, max, ignore) end

---EVERY node carrying script `kind`, as script handles in scene order — for
---picking among several instances (a camera finding the one third_person
---that is `net.isMine`, out of many player avatars).
---@param kind string
---@return table[]
function findScripts(kind) end

---The player's accessibility settings (`floptle/0079`). A game's options menu
---drives these; the engine honours the parts it owns (UI text sizes reflow, the
---colour filter is a post stage, UI transitions snap). Persist them with `save.*`.
---@class Access
---@field textScale fun(): number The UI text multiplier (1.0 = normal).
---@field setTextScale fun(scale: number) Set the UI text multiplier, 0.5–3.0. Applied BEFORE layout, so text scaling reflows rather than clipping. Out of range raises.
---@field colorFilter fun(): string The active colour-vision filter: \"none\" / \"protanopia\" / \"deuteranopia\" / \"tritanopia\".
---@field setColorFilter fun(name: string, strength?: number) Correct the picture for a colour vision deficiency (a post-chain stage, so it applies to everything the player sees). An unrecognised name raises.
---@field colorFilterStrength fun(): number How strongly the filter applies, 0–1.
---@field filters fun(): { name: string, label: string }[] Every filter in menu order, for a settings dropdown.
---@field reducedMotion fun(): boolean The player asked for less movement — read this for YOUR camera shake and screen effects.
---@field setReducedMotion fun(on: boolean) Ask for less movement. The engine snaps its own UI transitions.
---@field captions fun(): boolean Is the player showing captions?
---@field setCaptions fun(on: boolean) Turn captions on.
access = {}

---Say a caption line, if the player asked for captions — drawn by the engine
---bottom-centre at the player's text scale. A no-op returning false while
---`access.captions()` is off, so you write this beside the sound with no `if`.
---@param text string
---@param seconds? number How long it stays up; without one, the length of the line decides.
---@return boolean shown
function caption(text, seconds) end

---EVERY node carrying tag `tag` (Inspector \"tags\" chips / node:addTag), as
---node handles in scene order — an empty table when none.
---`findTagged(\"enemy\")[1]` grabs the first.
---@param tag string
---@return Node[]
function findTagged(tag) end

---A 3-component vector value with real operators: `a + b`, `a - b`, `v * 2`,
---`v / 2`, `-v`, `a == b`. Anything that ACCEPTS a vector also accepts a plain
---{x=, y=, z=} table or a node handle.
---@class Vec3
---@field x number
---@field y number
---@field z number
---@field length fun(self: Vec3): number
---@field lengthSquared fun(self: Vec3): number
---@field normalized fun(self: Vec3): Vec3 Unit-length copy (zero stays zero).
---@field dot fun(self: Vec3, other: Vec3): number
---@field cross fun(self: Vec3, other: Vec3): Vec3
---@field lerp fun(self: Vec3, other: Vec3, t: number): Vec3
---@field distance fun(self: Vec3, other: Vec3): number

---A 2-component vector (UI/screen math) — same operators as Vec3.
---@class Vec2
---@field x number
---@field y number
---@field length fun(self: Vec2): number
---@field lengthSquared fun(self: Vec2): number
---@field normalized fun(self: Vec2): Vec2
---@field dot fun(self: Vec2, other: Vec2): number
---@field lerp fun(self: Vec2, other: Vec2, t: number): Vec2
---@field distance fun(self: Vec2, other: Vec2): number

---Make a vec3: `vec3()` = zero, `vec3(s)` = splat, `vec3(x, y, z)`, or
---`vec3(other)` = copy (also from a {x=,y=,z=} table or node).
---@param x number|Vec3|Node|nil
---@param y number|nil
---@param z number|nil
---@return Vec3
function vec3(x, y, z) end

---Make a vec2: `vec2()` = zero, `vec2(s)` = splat, `vec2(x, y)`.
---@param x number|Vec2|nil
---@param y number|nil
---@return Vec2
function vec2(x, y) end

---Distance between two points: vectors, {x=,y=,z=} tables, or NODE handles —
---`distance(node, target)` just works. Also `distance(x1,y1,z1, x2,y2,z2)`.
---@param a Vec3|Vec2|Node|number
---@param b Vec3|Vec2|Node|number|nil
---@return number
function distance(a, b, ...) end

---The contact info passed to collision/trigger hooks: world point + normal.
---@class Hit
---@field x number Contact point X (world).
---@field y number Contact point Y (world).
---@field z number Contact point Z (world).
---@field nx number Contact normal X (unit, out of the hit surface).
---@field ny number Contact normal Y.
---@field nz number Contact normal Z.

---Fires the tick two nodes START touching (this node's body vs a solid
---collider, or vs another body). `other` is the other node's handle.
---@param node Node
---@param other Node
---@param hit Hit
function onCollisionEnter(node, other, hit) end

---Fires every tick while the touch lasts (resting on the ground reports its
---floor node every tick — gate on `other:hasTag(...)` etc.).
---@param node Node
---@param other Node
---@param hit Hit
function onCollisionStay(node, other, hit) end

---Fires the tick the pair separates (hit = the last known contact).
---@param node Node
---@param other Node
---@param hit Hit
function onCollisionExit(node, other, hit) end

---Fires the tick a body ENTERS a trigger (a Collider with the \"trigger\"
---switch on: no blocking, events only — portals, pickup zones, checkpoints).
---@param node Node
---@param other Node
---@param hit Hit
function onTriggerEnter(node, other, hit) end

---Fires every tick a body stays inside the trigger.
---@param node Node
---@param other Node
---@param hit Hit
function onTriggerStay(node, other, hit) end

---Fires the tick a body LEAVES the trigger.
---@param node Node
---@param other Node
---@param hit Hit
function onTriggerExit(node, other, hit) end

---Immediate-mode debug drawing (play mode): shapes show for ONE frame in the
---viewport, Scene AND Game views. Call every frame you want a shape visible.
---Colors are optional 0-1 floats (default green).
---@class Gizmo
gizmo = {}
---A world-space debug line.
---@param x1 number
---@param y1 number
---@param z1 number
---@param x2 number
---@param y2 number
---@param z2 number
---@param r? number
---@param g? number
---@param b? number
function gizmo.line(x1, y1, z1, x2, y2, z2, r, g, b) end
---A debug ray: origin + direction. With len the direction is normalized and
---the ray is that long (mirrors raycast) — great for visualizing ground checks.
---@param ox number
---@param oy number
---@param oz number
---@param dx number
---@param dy number
---@param dz number
---@param len? number
---@param r? number
---@param g? number
---@param b? number
function gizmo.ray(ox, oy, oz, dx, dy, dz, len, r, g, b) end
---A wire debug sphere (three rings): trigger zones, blast radii, ranges.
---@param x number
---@param y number
---@param z number
---@param radius? number
---@param r? number
---@param g? number
---@param b? number
function gizmo.sphere(x, y, z, radius, r, g, b) end
---A small 3-axis cross marking a spot: hit points, waypoints, spawns.
---@param x number
---@param y number
---@param z number
---@param size? number
---@param r? number
---@param g? number
---@param b? number
function gizmo.point(x, y, z, size, r, g, b) end

---Scene management: the running scene and transitions between scenes.
---In multiplayer only the SERVER may switch — every client follows
---automatically (a joined client's `scene.load` is refused; send the server
---an RPC and let its script decide).
---@class Scene
scene = {}
---Queue a transition to another scene, performed at the next frame boundary:
---the world swaps to the new scene, physics/animators/particles/audio rebuild
---against it, and every script's `start` re-fires — exactly like the scene
---booting fresh. Accepts a name (\"arena\"), a scenes-relative path
---(\"arenas/desert\"), or a project-relative path (\"scenes/arena.ron\").
---
---With `{ additive = true }` the scene is LAYERED on top of the running one
---instead of replacing it: nothing is torn down, no script restarts, and the
---new nodes join the live physics sim. An additive scene brings nodes only —
---no second lighting, skybox or post-processing node.
---@param name string
---@param opts? { additive?: boolean }
function scene.load(name, opts) end
---Remove an additively-loaded scene (and anything parented under it). The
---base scene — the one you opened — is never a candidate.
---@param name string
function scene.unload(name) end
---Be told when a scene has finished loading — AFTER the world is whole, which
---is when a loading screen's job is done. The callback receives the scene's
---name and whether it arrived additively.
---
---The subscription dies with the script that made it, so a node covering a
---full swap must set `node.persistent = true` to be around for the answer.
---@param fn fun(name: string, additive: boolean)
function scene.onLoaded(fn) end
---The running scene's name (its file stem, e.g. \"first\").
---@return string
function scene.current() end
---Every scene in the project, as names `scene.load` accepts (sorted;
---subfolders kept, e.g. \"arenas/desert\").
---@return string[]
function scene.list() end

---Thousands of props from a seed, GPU-instanced and never scene nodes. Your
---generator keeps deciding WHAT grows where; the engine places and draws it.
---@class Scatter
scatter = {}
---Declare a scatter source; returns its id. `asset` (a mesh path) or a `lod`
---list is required; everything else defaults. An option this doesn't list is an
---ERROR, not a shrug — scattered props have no collision, and the `collide`
---option that suggested otherwise was never read by anything.
---
---Give `center` + `radius` for a planet's surface, or `center` + `halfX`/`halfZ`
---for a flat region.
---
---`asset` is a mesh file **or** a `.prefab.ron`. A prefab is baked ONCE into one
---instanced draw per Mesh node it contains, each at its authored place within
---the prop — so a plant your own generator assembled (a trunk and three fronds)
---can be scattered without a scene node per frond. Nodes that are not Meshes are
---skipped; a prototype that yields nothing says so in the Console.
---
---`density` gives a world biomes: a function(x, y, z) returning 0..1, sampled
---ONCE when the source is declared into a `densityRows` grid. It is not called
---per instance and never while chunks build — placement has to stay a pure
---function of the seed, or walking away and back would regrow a different
---world. Density 0 generates no instance at all.
---@param opts { asset?: string, lod?: { asset: string, distance: number }[], seed?: number, center?: Vec3, radius?: number, halfX?: number, halfZ?: number, perChunk?: number, chunk?: number, align?: string, scaleMin?: number, scaleMax?: number, range?: number, fade?: number, density?: fun(x: number, y: number, z: number): number | number[], densityRows?: number }
---@return integer
function scatter.create(opts) end
---Instances within `radius` of a point, nearest first. What a harvest verb aims
---with — a proximity query, not a ray.
---@param id integer
---@param point Vec3
---@param radius? number
---@return { id: integer, distance: number, pos: Vec3, scale: number, param: number }[]
function scatter.near(id, point, radius) end
---Remove one instance, permanently. By ID, so it survives the chunk streaming
---out and back in.
---@param id integer
---@param instanceId integer
---@return boolean
function scatter.remove(id, instanceId) end
---Put one instance back, or all of them — what regrowth is made of.
---@param id integer
---@param instanceId? integer
---@return integer restored
function scatter.restore(id, instanceId) end
---The instance ids this source has lost. Save THIS (a handful of numbers), not
---every prop you ever saw.
---@param id integer
---@return integer[]
function scatter.removed(id) end
---Drop a whole source.
---@param id integer
---@return boolean
function scatter.destroy(id) end

---The scene's bodies of water. The engine floats things and drags them; what
---being WET means — swimming, drowning, a flooded engine, a gauge going red —
---is the game's, and all of it comes from one number: the depth.
---@class Water
water = {}
---Metres below the nearest water surface; 0 in air. Takes (x, y, z), a vec3,
---or a node. The same rule the solver uses, so a swim state can never disagree
---with the physics floating you.
---@param x number|Vec3|Node
---@param y? number
---@param z? number
---@return number
function water.depthAt(x, y, z) end
---nil in air, else `{depth, density, frozen, node, up}` — `up` being the
---direction OUT of the water (radial on a sea; NOT −gravity in a tilted tank).
---@param x number|Vec3|Node
---@param y? number
---@param z? number
---@return { depth: number, density: number, frozen: boolean, node: Node, up: Vec3 }|nil
function water.at(x, y, z) end
---The yes/no, when that is all you wanted.
---@param x number|Vec3|Node
---@param y? number
---@param z? number
---@return boolean
function water.isUnderwater(x, y, z) end
---Freeze or thaw a water volume. Frozen water applies no buoyancy, no drag and
---no underwater look — pair it with a Collidable surface and a sea becomes
---walkable ground.
---@param node Node
---@param frozen boolean
function water.setFrozen(node, frozen) end
---Every water volume in the scene, as nodes.
---@return Node[]
function water.volumes() end

---Runtime terrain editing + queries (Terrain 2.0). Edits queue and land the
---same tick (collision updates with the surface). World coordinates.
---In multiplayer, run edits on the SERVER and mirror them with an RPC that
---repeats the same call — the ops are deterministic.
---@class Terrain
terrain = {}
---Sculpt the nearest terrain at (x,y,z): mode \"raise\" (default), \"lower\"/\"dig\",
---\"smooth\", or \"flatten\". strength 0..1 (default 1). No-op when no terrain
---surface is near the point.
---@param x number
---@param y number
---@param z number
---@param radius number
---@param strength? number
---@param mode? string
---@return number id an id for the yield report this edit will produce
function terrain.sculpt(x, y, z, radius, strength, mode) end
---Dig a hole — sugar for `terrain.sculpt(x, y, z, radius, strength, \"lower\")`.
---@param x number
---@param y number
---@param z number
---@param radius number
---@param strength? number
---@return number id an id for the yield report this edit will produce
function terrain.dig(x, y, z, radius, strength) end
---Recolor the terrain surface inside the brush ball (r/g/b are 0..1).
---@param x number
---@param y number
---@param z number
---@param radius number
---@param r number
---@param g number
---@param b number
---@param strength? number
function terrain.paint(x, y, z, radius, r, g, b, strength) end
---Paint a terrain-palette texture slot (1-based, the Terrain tab's palette;
---0 clears back to the flat color).
---@param x number
---@param y number
---@param z number
---@param radius number
---@param slot number
function terrain.paintTexture(x, y, z, radius, slot) end

---REPLACE terrain volume `id`'s whole field with a generated planet — the
---engine's generic heavy procgen primitive (sphere ± noise relief, caves with
---galleries + chambers, solid molten core, impact craters, layered materials).
---Runs on an editor background thread (seconds per body; Console shows
---progress). Every knob optional; layer paints are {slot=…, color={r,g,b}}:
---radius, voxel, relief, bumpFreq, caveDepth, coreR, corePaint, craters,
---craterMin, craterMax, craterDust, surfaceA, surfaceB, patchBias, patchThr,
---subsoil, subsoilDepth, strata, strataDepth, deep,
---pockets {slot,color,threshold,minDepth}, seam {slot,color,minDepth,center,width},
---iceCaps {lat,slot,color}, seed.
---@param id number The terrain id (a node with that Terrain id shows the result).
---@param opts? table
function terrain.generatePlanet(id, opts) end
---Set (or read, with no argument) the game's SAVE-SLOT directory for
---player-edited terrain, relative to the project root (e.g.
---\"saves/slot1/terrain\"). While set, streaming loads a body's field from here
---FIRST — before the project file or its genspec — and writes edited fields
---back here when bodies stream out, so a player's digs persist per save slot
---without touching authored project data. Pass \"\" to clear; cleared
---automatically when Play stops.
---@param path? string
---@return string|nil
function terrain.saveDir(path) end
---Keep the named body's terrain RESIDENT this frame regardless of where the
---ship/player physically is: it streams in if cold and never streams out.
---Immediate mode — call it every frame while you care (the map calls it for
---its focused planet). Streaming is otherwise anchored to the PHYSICAL
---positions of dynamic bodies, never the camera.
---@param bodyName string The body's node name (as in `space.bodies()`).
function terrain.warm(bodyName) end
---Checkpoint every EDITED resident terrain field to the save slot
---(`terrain.saveDir` must be set). Runs IN THE BACKGROUND — a few chunks of
---encoding per frame plus a threaded write, deferred while a field is being
---actively dug — so autosaves never stutter the game. Exit paths (Stop,
---scene.load out of the slot) finish the writes synchronously, so a
---checkpoint is never lost. Streaming also flushes when bodies stream out.
function terrain.flush() end
---Delete a save slot's persisted terrain directory from disk (pair with
---`save.deleteSlot` in a \"delete this save\" UI). Narrow by design: the path
---must be relative with no \"..\", must not be the ACTIVE `terrain.saveDir`
---(clear it first), and only terrain files (.cfield/.tfield/.meta) in that one
---directory are removed — the emptied directory (and an emptied parent) is
---then tidied away. Returns the number of files removed.
---@param path string e.g. \"saves/slot2/terrain\"
---@return number
function terrain.deleteSaveDir(path) end
---Signed distance from (x,y,z) to the nearest terrain surface (negative =
---inside rock), or nil when the scene has no terrain.
---@param x number
---@param y number
---@param z number
---@return number|nil
function terrain.query(x, y, z) end
---The texture-palette slot at a world point — what the rock there is made of —
---or nil where the field carries no slot.
---@param x number
---@param y number
---@param z number
---@return number|nil
function terrain.slotAt(x, y, z) end
---Everything inside a sphere: a list of hits, deepest overlap first. Hits carry
---the same fields a raycast hit does; `distance` is the PENETRATION DEPTH.
---Sees sensors (a hitbox wants to know it swept a trigger) and, inside
---`net.rewind`, sees the rewound world.
---@param center Vec3
---@param radius number
---@param opts? table { ignore = node, layers = \"Ground\" or a list of names }
---@return table[]
function overlapSphere(center, radius, opts) end
---Sweep a sphere along a ray; the first thing it touches, or nil. Catches what a
---bare ray squeaks past.
---@param origin Vec3
---@param dir Vec3
---@param radius number
---@param max number
---@param opts? table
---@return table|nil
function spherecast(origin, dir, radius, max, opts) end
---Sweep an upright capsule — the shape a character actually is — along a ray.
---@param origin Vec3
---@param dir Vec3
---@param radius number
---@param halfHeight number
---@param max number
---@param opts? table
---@return table|nil
function capsulecast(origin, dir, radius, halfHeight, max, opts) end
---Reports for terrain edits that have LANDED since the last call (drained).
---Each entry is { id, removed, added, untextured, slots = { [slot] = volume } }
---in world cubic units; `removed` equals `untextured` plus the slot volumes.
---An edit is queued and applied after the script pass, so a report arrives on a
---later frame than the `terrain.dig` that asked for it — match them by `id`.
---@return table[]
function terrain.yields() end
---World Y of the highest terrain surface under (x,z), or nil when no terrain
---is hit there.
---@param x number
---@param z number
---@return number|nil
function terrain.height(x, z) end

---Seeded value noise, one octave, ≈ -1..1. Deterministic on every machine —
---the SAME numbers the engine's Rust generators produce. Scale the inputs to
---@class mathlib
---@field clamp fun(x: number, lo: number, hi: number): number Hold x inside lo..hi (reversed bounds tolerated).
---@field saturate fun(x: number): number Clamp to 0..1.
---@field sign fun(x: number): number -1, 0 or 1 (exactly 0 for 0).
---@field round fun(x: number, step?: number): number Nearest whole number, or nearest multiple of `step` (`round(x, 0.25)` snaps to quarters).
---@field lerp fun(a: number, b: number, t: number): number Linear blend, UNCLAMPED (extrapolates).
---@field mix fun(a: number, b: number, t: number): number lerp with t clamped to 0..1.
---@field inverseLerp fun(a: number, b: number, x: number): number Where x sits in a..b, 0..1 (0 when a == b, never NaN).
---@field remap fun(x: number, a: number, b: number, c: number, d: number): number Range a..b onto c..d.
---@field smoothstep fun(a: number, b: number, x: number): number 0..1 with eased ends.
---@field approach fun(current: number, target: number, maxDelta: number): number Move toward target without overshooting — pass `rate * dt`.
---@field wrapAngle fun(a: number): number An angle folded into (−pi, pi].
---@field deltaAngle fun(a: number, b: number): number The SHORTEST signed turn from a to b, correct across the ±pi seam.
---@field approachAngle fun(current: number, target: number, maxDelta: number): number approach() for headings — turns the short way, never overshoots.
---@field pingPong fun(t: number, len: number): number 0 → len → 0, forever.
math = math

---@class tablelib
---@field map fun(list: table, fn: fun(v: any, i: number): any): table A new list of fn(value, i).
---@field filter fun(list: table, fn: fun(v: any, i: number): boolean): table A new list of the items fn accepts.
---@field find fun(list: table, fn: fun(v: any, i: number): boolean): any, number|nil The first item satisfying the predicate, and its index.
---@field indexOf fun(list: table, value: any): number|nil The index of a value by equality.
---@field count fun(t: table, fn?: fun(v: any, i: number): boolean): number Entries (keyed tables too), or how many match.
---@field sum fun(list: table, fn?: fun(v: any, i: number): number): number Add the numbers, or add fn over them.
---@field keys fun(t: table): table The keys as a SORTED list (pairs order isn't reproducible).
---@field copy fun(t: table): table A shallow copy.
---@field extend fun(dst: table, src: table): table Append src's items onto dst in place; returns dst.
---@field reverse fun(list: table): table A new list, back to front.
table = table

---pick a frequency (lattice cell = 1 unit).
---@param x number
---@param y number
---@param z number
---@param seed? number
---@return number
function math.noise(x, y, z, seed) end

---Seeded fractal noise (fbm): `octaves` layers (default 4), rotated so features
---never align to the axes. ≈ -1..1, deterministic everywhere.
---@param x number
---@param y number
---@param z number
---@param octaves? number
---@param seed? number
---@return number
function math.fbm(x, y, z, octaves, seed) end

---A deterministic random stream: the same seed gives the same sequence on
---every machine. Use for gameplay that must reproduce (loot, procgen scatter,
---anything a server might replay); `math.random` stays for throwaway rolls.
---@class Rng
---@field next fun(self: Rng): number Uniform in [0, 1).
---@field range fun(self: Rng, a: number, b: number): number Uniform in [a, b).
---@field int fun(self: Rng, a: number, b: number): integer Uniform integer in [a, b] inclusive.
---@field pick fun(self: Rng, list: any[]): any A uniform element of `list` (nil if empty).

---Make a deterministic random stream. NO seed = seeded from the clock (a
---fresh roll every call) — read `r.seed` to reproduce it later.
---@param seed? number
---@return Rng
function rng(seed) end

---Persistent game data: a per-slot key→value store that survives Play
---sessions, editor restarts, and ships with exported builds. Values take the
---synced-var guardrails (numbers/strings/bools/tables ≤ depth 4, ≤ 1 KB).
---Flushes on Stop + every few seconds during Play; `save.flush()` forces it.
---Multiplayer: LOCAL storage — for server-authoritative progress call save.*
---on the server and hand results to clients via synced/RPC.
---@class Save
save = {}
---Store a value under `key` (guardrails apply; violations are script errors).
---@param key string
---@param value any
function save.set(key, value) end
---The stored value, else `default`, else nil.
---@param key string
---@param default? any
---@return any
function save.get(key, default) end
---Remove a key. Returns true if something was removed.
---@param key string
---@return boolean
function save.delete(key) end
---Switch the active save slot (flushing the old one), or read the current
---slot's name when called with no argument. Names: letters/digits/-/_ only.
---@param name? string
---@return string
function save.slot(name) end
---Delete a slot's store file from disk (\"delete this save\" UIs). Deleting
---the ACTIVE slot also empties the in-memory store, so the slot is instantly
---reusable as a fresh save. Per-slot terrain is a separate directory — pair
---with `terrain.deleteSaveDir`. Returns true if a file was removed.
---@param name string
---@return boolean
function save.deleteSlot(name) end
---Write the store to disk now (checkpoints). Returns false on an IO error
---(also surfaced in the Console).
---@return boolean
function save.flush() end

---A scheduled timer. `cancel()` aborts it (safe to call after it fired).
---@class TimerHandle
---@field cancel fun(self: TimerHandle)

---Run `fn` once after `seconds` of GAME TIME (tick-driven and deterministic;
---paused when the game is paused). The callback gets no arguments — capture
---what you need as locals. Errors log to the Console and kill only that timer.
---@param seconds number
---@param fn fun()
---@return TimerHandle
function after(seconds, fn) end

---Run `fn` repeatedly, first after `seconds`, then every `seconds` (anchored:
---long sessions don't drift). Cancel via the returned handle.
---@param seconds number
---@param fn fun()
---@return TimerHandle
function every(seconds, fn) end

---Animate: call `fn(alpha)` every tick for `seconds`, alpha easing 0→1, final
---call guaranteed exactly at 1.0. `ease` is \"linear\" (default), \"smooth\",
---\"in\", or \"out\". e.g. `tween(0.5, function(a) node.y = a * 3 end, \"smooth\")`
---@param seconds number
---@param fn fun(alpha: number)
---@param ease? \"linear\"|\"smooth\"|\"in\"|\"out\"
---@return TimerHandle
function tween(seconds, fn, ease) end

---One celestial body from this tick's on-rails snapshot (world coords).
---@class SpaceBody
---@field name string
---@field x number
---@field y number
---@field z number
---@field vx number
---@field vy number
---@field vz number
---@field mu number Gravitational parameter µ = GM.
---@field radius number Physical surface radius.
---@field soi number Sphere-of-influence radius (-1 = infinite, the root).

---Orbital mechanics readouts (scenes with Celestial Body components): planets
---Sim-wide physics controls.
---@class Physics
physics = {}
---Pause / resume the whole physics step (scripts, rails and streaming keep
---running) — loading screens, cutscenes, pause menus. Queued thrust is
---dropped, never banked, while paused.
---@param on boolean
function physics.pause(on) end
---Whether the physics step is currently paused.
---@return boolean
function physics.isPaused() end
---Frame-step: freeze the whole gameplay tick and release exactly `n` ticks (default 1),
---each advancing scripts, physics and animation one frame. The scriptable half of the
---editor's ⏭ Step button, for a training mode's own frame stepper. Call it from
---`update` — the frame pass still runs while the tick is frozen, `fixedUpdate` does not.
---@param n integer|nil
function physics.step(n) end

---ride exact Kepler rails; one dominant body pulls µ/r² (patched conics).
---@class Space
space = {}
---Space time in seconds (advances with warp; 0 at Play start).
---@return number
function space.time() end
---Read the warp multiplier, or request one (1 .. 100000). Rails fast-forward;
---local physics keeps ticking at 1×.
---@param mult? number
---@return number
function space.warp(mult) end
---Every celestial body this tick.
---@return SpaceBody[]
function space.bodies() end
---One body by node name, or nil.
---@param name string
---@return SpaceBody|nil
function space.body(name) end
---The dominant body's name at a world position (deepest SOI), or nil.
---@return string|nil
function space.dominant(x, y, z) end
---Gravitational acceleration (µ/r² toward the dominant body) at a position.
---@return number, number, number
function space.gravity(x, y, z) end
---The orbit (conic) a craft at position+velocity is on around its dominant
---body: { body, a, e, periapsis, apoapsis?, period? } — apoapsis/period absent
---on an escape trajectory. Distances from the body CENTER. Pass `node.vx/vy/vz`
---straight through: a body's velocity is ALREADY measured in its dominant
---celestial's frame (do NOT subtract the body's world velocity).
---@return table|nil
function space.elements(x, y, z, vx, vy, vz) end
---Propagate a state vector along its two-body conic about a point mass `mu`:
---returns the position AND velocity `px,py,pz, vx,vy,vz` exactly `dt` seconds
---later (elliptic OR hyperbolic — no integration drift). The primitive maneuver
---nodes and patched-conic encounter-finding are built from: the state is in
---whatever frame you pass, so compose parent frames yourself (add the
---attractor's own motion for a moon-of-a-planet). Degenerate input passes
---through unchanged.
---@param px number @param py number @param pz number
---@param vx number @param vy number @param vz number
---@param mu number @param dt number
---@return number, number, number, number, number, number
function space.propagate(px, py, pz, vx, vy, vz, mu, dt) end
";

/// `.luarc.json` pointing the Lua language server at the annotation library and
/// declaring the engine globals (so they aren't flagged undefined).
pub(crate) const LUARC_JSON: &str = "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"camera\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"terrain\", \"rng\", \"save\", \"after\", \"every\", \"tween\", \"space\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\", \"snapshot\", \"restore\"]\n}\n";

/// Byte-exact PREVIOUS engine-generated `.luarc.json` versions: a project file
/// matching one of these was never hand-edited, so it's safe to migrate to the
/// current `LUARC_JSON` (a customized file is always left alone).
const LUARC_JSON_OLD: &[&str] = &[
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"terrain\", \"rng\", \"save\", \"after\", \"every\", \"tween\", \"space\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"terrain\", \"rng\", \"save\", \"after\", \"every\", \"tween\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"terrain\", \"rng\", \"save\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"terrain\", \"rng\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"terrain\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findTagged\", \"access\", \"caption\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"createNode\", \"destroy\", \"spawnEffect\", \"draw\", \"scene\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
];

/// Write the Lua language-server support files into a project (annotations always
/// refreshed; `.luarc.json` only if absent OR still an unmodified engine-generated
/// version — a user's own config is preserved).
pub(crate) fn write_lua_support(project_root: &Path) {
    let lib = project_root.join(".floptle").join("library");
    let _ = std::fs::create_dir_all(&lib);
    let _ = std::fs::write(lib.join("floptle.lua"), LUA_ANNOTATIONS);
    let luarc = project_root.join(".luarc.json");
    let migrate = match std::fs::read_to_string(&luarc) {
        Ok(cur) => LUARC_JSON_OLD.contains(&cur.as_str()),
        Err(_) => true, // absent
    };
    if migrate {
        let _ = std::fs::write(luarc, LUARC_JSON);
    }
}

/// Write the default scripts into `scripts_dir` (each only if absent).
pub(crate) fn seed_default_scripts(scripts_dir: &Path) {
    let _ = std::fs::create_dir_all(scripts_dir);
    for (name, body) in DEFAULT_SCRIPTS {
        let p = scripts_dir.join(name);
        if !p.exists() {
            let _ = std::fs::write(&p, body);
        }
    }
}
