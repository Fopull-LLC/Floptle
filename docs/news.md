## Just shipped

**v0.54.0 "Where The Time Goes"** — a polished surface can finally be a mirror.
Roughness 0 was sharp and everything above it fell off a cliff, so the only crisp
reflection available was the one at exactly zero; the blur now follows the
highlight the surface actually has, and a reflection of something close stays
sharper than one of something far. **Project Settings ⏵ Rendering ⏵ Reflection
detail** gives a probe four times the detail it used to capture, and two mirrors
facing each other settle instead of climbing into a white blob
(**Lighting ⏵ brightness cap**). New **Window ⏵ ⏱ Frame timing** says which part
of a frame is slow rather than only that it is — pointed at a Backrooms-style
interior it found volumetric fog asking every lamp for a full surface-lighting
calculation and using two numbers out of it, and that scene now renders **2.6×
faster**. Children parented to anything that is not a folder no longer disappear
from the Hierarchy when a project is reopened.

**v0.53.1** — the Scene view and the Game view agree again: reflections, contact
shadows, shoreline foam and lamp shadows could be missing from one while working
in the other, and could start working on their own after a resize. And if a small
scene has been running at a stubbornly round frame rate, that is the display
accepting one frame in three rather than the engine struggling — **Project
Settings ⏵ Rendering ⏵ Frame pacing** lets you escape it, and the window title
now shows what a frame actually costs beside the rate, so the two are never
confused again.

**v0.53.0 "Inside The Room"** — three things the renderer could only do for what
was on screen, now done for what is not. Drop a **◐ Reflection Probe** into a
room, size its box to the walls, and every reflective surface inside it shows
that room instead of the sky — a mirror indoors used to reflect daylight through
a sealed ceiling, and there was no setting that changed it. A casting lamp is now
blocked by a wall whether you are facing the wall or facing away from it, with
soft edges that come from how big the lamp is rather than from a knob. And
**Lighting ⏵ glass layers** lets you see through more than one pane at a time, so
a window can have a bottle standing behind it and a fish tank can be six panes
rather than one box.

**v0.52.0 "What You See"** — a polished floor shows the room standing on it, and
a crystal ball shows the room *through* it, upside down. Turn **reflections** on
in the Lighting node and every reflective surface picks up the scene rather than
only the sky; turn **see-through** up on a material and light passes through it,
bent by the index of refraction, frosted by the roughness and tinted by the
material's own colour. Lamps can cast shadows now, per light — a torch in a
doorway used to light the room behind the door exactly as brightly, and there was
no setting that changed it. And the docked Game panel finally shows the same game
the window does: contact shadows, shoreline foam, reflections and lamp shadows
were all simply missing from it, so the small panel you work in disagreed with
the window you test in. The border of editor-view bleed around it is gone too.
Copying a rotation keyframe no longer teleports the object to the world origin,
bones are octahedra you can click anywhere along, and the Model tool grew loop
cut, bevel, ring select, Ctrl+A and Blender-style Ctrl+click path selection.

**v0.51.0 "Ask Once"** — the PS1-era look is a **project setting** now instead of
four checkboxes on every material you own. Project Settings ⏵ Rendering ⏵ Era
artefacts turns vertex jitter, affine textures, vertex lighting and screen-door
transparency on for everything that draws — models, primitives, tilemaps, map
geometry, terrain, characters, including the surfaces that never named a
material at all, which is most of a level. Jitter is offered as named strengths
— **off / pixels / chunky / heavy** — measured against your own pixel
resolution, so "pixels" means the same thing whether you render at 240 rows or
480 and you never have to work out which number suits your target. One thing
worth knowing if you have been staring at a paused scene wondering whether it is
on: jitter is a **snap, not a shimmer**. A still camera on a still object lands
in the same grid cell every frame and holds perfectly still, exactly as the
hardware did — move something to see it. Any material can take **none** of it
(*project artefacts ⏵ opt out*), which is how a first-person weapon or a sky
shell holds steady in a world that wobbles. And every material gained a **fog**
switch: turn it off and that surface draws at its own colour however far away it
is, for the things that are not really in the world at that distance — a
viewmodel a metre from your eye, a backdrop card, a marker that has to stay
readable through the weather. Nothing to do on upgrade; every new setting starts
at the value that changes nothing.

**v0.50.1** — opening a scene whose UI uses a `stage ui` shader killed the
editor before it drew a frame; if you have a custom meter, gauge or instrument
built that way, take this one. It had been broken since the release that made
the scene render in real light — the shader's world-space pipeline was still
built for the window's 8-bit format while the scene it draws into is now HDR —
and you would only meet it once 0.50.0 fixed the textured-menu crash that used
to happen first. Behind it, a change so the next one is not a crash at all:
when the graphics driver refuses a draw, the editor now names it in the Console
and keeps rendering instead of ending the session, and a crash report keeps the
**first** failure rather than the last thing to fall over on the way down.

**v0.50.0 "Show Your Work"** — if you have a project that worked on 0.41.1 and
died on 0.49.0, this is the one to take: a UI element drawing an image from your
project crashed the game on the frame that element first appeared, so any menu
with a logo, a custom button face or an item icon was gone on its first frame.
Open the project and it draws. The rest is the shader graph, which now shows you
what your shader is doing. A **texture slot carries its own image**, picked right
on the node — before this a slot was a name and nothing else, so the slot's
thumbnail, the `sample()` reading it and the final colour all came back as a grey
checkerboard, and the one node whose whole job is to bring a picture in was the
one node you could not look at. That image binds wherever a material leaves the
slot empty, so a texture shader looks right the moment you assign it. A new **▣
focus panel** shows the selected node large with its type, what the op does,
where each input comes from, and how its preview is being drawn — a float is
grey, a vec2 is red and green, an sdf is a distance read-out — which is the
difference between a wall of thumbnails and a picture you can read. **Let a wire
go over empty canvas** and the palette opens there and connects what you pick;
**Ctrl+C / Ctrl+V** carries a chunk of one shader into another, knobs and texture
slots included. Baked lighting finally has a way in: **Add → ☀ Light Probes**,
which shipped in 0.49.0 with no menu entry to create it. And the Shaders tab
stops redrawing every node's preview every frame when nothing has changed.

**v0.49.0 "All At Once"** — everything since 0.41.1, in one release. Materials
have a **Physical** shading model with roughness, metallic and four surface maps,
so metal behaves like metal. The scene is rendered in real light all the way
through and only turned into a picture at the end, which brings a **tonemap**, a
full **colour grade**, **depth of field** with a node to follow and polygonal
bokeh, a **lens** (aberration, distortion, sharpen, denoise, grain), and
**screen shaders** — your own full-screen passes, in order, each one getting the
finished frame plus its depth and normals. Drop a **Light Probes** volume and
press Bake and light coming off a red wall lands red on the floor beside it.
**Fog is lit by the scene**: the sun behind a bank lights it, a lamp carried into
it glows, and a shadow crossing it stays a shadow — which is where beams through
a window come from. A light can be a **shape** — point, sphere, rect, disk or
tube — so a four-metre window lights a wall evenly instead of leaving a hot spot
at its centre. **Contact shadows** put the small dark line back under a foot, in
a seam, behind a bolt, from what is actually on screen rather than from a
capsule. And new in this one: **motion blur** on the camera, **multi-select
editing** (change roughness once for twelve crates and only roughness travels),
**clickable bones** in the Scene view, and **friction that works** — a ramp now
holds a body while `tan(its angle) ≤ friction` and lets go above it, instead of
everything creeping downhill forever. Two things change on their own: physics
friction, and volumetric fog arriving lit. Everything else is off until you turn
it on.

**v0.41.1 "Draw The Ramp"** — tile collision you draw by hand, including
**slopes**. Pick **shape** under TILE and draw the collider onto the tile: click
an edge to add a point, drag to move, right-click to remove, four 45° ramps on a
button. Points snap to the art's own pixel grid, which is what makes a slope
built from several tiles actually meet instead of catching a character on every
boundary — and it collides as the shape you drew, not the box around it, concave
outlines included. Everything under TILE now applies to **every selected tile**:
drag a box, or ctrl-click tiles that are not next to each other, and set the
collision once. The autotile picker got two fixes — its 3×3 neighbourhood diagram
was **upside down**, so a tile answering "more of this group above me" was drawn
and described as *below*, and every shape is now **named** (*"waiting for: the
top-left corner"*). Switching between placing a tile and painting an autotile is
a **brush** row at the top of the palette rather than a side effect of which tile
you clicked. And a rigidbody has a **2D** switch: it keeps its depth, never
drifts out of the layer, still spins the one way a flat object spins — and
collides with the same world a 3D body does, because there is no separate 2D
physics engine to be missing features.

**v0.40.5 "Shine On"** — 2D light is smooth now, whatever else your scene is set
to. Posterize quantizes your **palette**: the set of values your art is allowed
to be. A light is not one of those values — it is a multiplier on whatever value
your art is — and while the quantize was the last thing to touch the frame, the
two were the same setting. That left no configuration that was right: hard
concentric rings, or a stipple that reads as a dither pattern, or no palette at
all. The quantize now runs over your art, before any light reaches it, so your
tiles land on their bands and your lights ride on top. A torch is a soft pool
again and a dark room gets darker smoothly. Nothing to switch on. The same rule
fixes the **vignette**, which is a smooth radial darkening and was arriving as
rings in the corners for exactly the reason a light was — everything
light-shaped, including bloom and ambient occlusion, is downstream of the
quantize now. And **dither the bands** is finally just what it sounds like: a way
to hold a gradient your palette can't. It does nothing to lighting. If you turned
it on to smooth your lights, or squashed a light's falloff into one band to stop
it ringing, you can undo both.

**v0.40.4 "Something In The Way"** — 2D light stops at walls. A light used to
reach the floor, the counter, the shelf and the floor behind the shelf all by
the same distance from the lamp, so what landed on screen was a disc of
brightness with the room drawn on top of it. **blocks light** does what the
Inspector has always said it would: under `auto` a tilemap casts exactly where
it is solid, from the colliders its tileset already declares, so a level's
collision *is* its light occlusion and the cover that stops a bullet is the
cover that stops the light. Nothing is rebuilt when a light moves, and a scene
with no casters pays what it always did. A light also has a shape now — **full
out to** holds it at full brightness before the ramp starts, and **falloff** is
that ramp's exponent — which is how a posterized game lands a whole light inside
one band instead of drawing concentric rings. And the rings that were **the
wrong colour** are fixed: posterize quantized each channel separately, so a mild
warm white at `{1.0, 0.86, 0.62}` — a torch, a lamp, a fire, a muzzle flash —
banded into olive and maroon rings nobody chose and produced no clean brightness
step anywhere in its radius. **PostProcess ▸ step brightness, keep colour**
steps once and carries the hue along; a grey pixel is identical either way.

**v0.40.1 "Give It Back"** — Escape gives you your cursor back during Play, and
this time it stays given. A first-person game takes the pointer, which is the
point of one; getting it back was the problem, because Escape released it for a
single frame and the game's next `update` took it straight back. Reaching the
Inspector mid-play meant tabbing out of the whole application, and clicking back
in handed the cursor straight over again. Now **Escape takes the pointer, a
click on the Game view hands it over**, leaving the window takes it too, and the
Game view says along its bottom edge which of you has it — a grabbed cursor is
invisible, so the thing that tells you how to get it back can't be the cursor.
While the editor holds it your game reads a **neutral mouse**, so tuning a value
in mid-flight doesn't spin the view across to the Inspector or fire a weapon
with every click on a slider. Its keyboard and gamepad are untouched; it's still
playing.

**v0.40.0 "As You Set It"** — the number you typed is the number that reaches the
screen. A tilemap or sprite at **alpha 0.72 was drawing at 0.92** in every 2D
project since v0.38, with no light placed and nothing switched on, because the 2D
lighting pass composited your flat surfaces a second time; opacity is yours again
at every value. **Your own font**, at last: `draw.text` takes one, and
**Project Settings ▸ UI font** sets it for every string that names none — a game
whose UI is a pixel font could not draw a single immediate-mode string in it
before, and the symptom read as bad letter spacing rather than as the wrong
typeface. **UI Layer ▸ text snap** keeps a pixel font on its own cell grid at
window sizes where the layer scale is fractional, which is most of them. A script
can now read and write the **2D base light**, so a brightness setting, a quality
governor and a blackout are all possible — and the rest of the Lighting node came
with it, for day cycles and weather. Three things that were right in the Scene
view and missing everywhere else are fixed: **water draws in the game**, a
**vertex-painted shape is painted in every view**, and **switching a node off now
switches its light off**. And 2D lighting costs nothing until a light of yours can
actually reach something — `perf.counts().flat2d` says how much it is costing.

**v0.39.1 "Only Ever Brighter"** — placing your first 2D light no longer darkens
the scene. The base brightness a flat surface got dropped from full to the 3D
ambient the moment any 2D light existed, so one light took a level to about 12%
and a tileset read as having vanished. The base is now its own **2D base light**
on the Lighting node, defaulting to white — turn it down when you want a dark
room, and a light can otherwise only add. Also: lit tilemaps no longer blink in
and out as the camera moves, and the docs now say plainly that there is no 2D
*directional* light.

**v0.39.0 "Ask The Sky"** — a procedural sky can finally answer to your game.
`setShaderParam` reaches a Skybox's uniforms, so a sky that catches fire during a
cutscene runs on your story beat instead of on a timer; the post chain opened up
the same way, so a cutscene can push a vignette without a second scene. Shaders
got **`atan2`** and the rest of the inverse trig, which is what a radial wipe, a
cooldown dial, a swirl or a skyline around the horizon all need and none of them
could do. Two invisible ceilings became visible: **sixteen lights** reach the
screen and the survivors are now chosen by what they contribute at the camera
rather than by luck, so a torch stops going out for no reason — and **one-shot
effects have a limit**, which matters because a per-frame spawn budget used to
cost twice as much on a 144 Hz monitor. `perf.counts()` reports what each cap
cut. Profiling can see particles and audio at all for the first time. And editing
during Play now tells you it will be discarded, with a copy/paste that survives
Stop so you can keep what you found.

**v0.38.0 "Turn The Lights On"** — 2D lights. Put a PointLight in a scene whose
camera is orthographic and it lights your tiles and sprites with a real falloff,
leaving meshes alone; a surface with no light near it sits at ambient, so a dark
room is dark and a torch carves a warm circle out of it. Each light lists the
**sorting layers** it reaches, so a torch can pass over a background without
lighting it — the one thing every 2D game wants. Every node carries a three-way
**2D light** setting (`auto`, `2d`, `3d`); only `auto` is decided for you, and it
shows you what it decided and why. Normal maps, shadows and height are not in
this one.

Also: an autotile **shape can hold as many tiles as you like** — variants, picked
by where the square is, so a field of grass varies and varies the same way every
time you open it — and **one tile can draw as many shapes as you like**. Assigning
a tile to a second shape used to silently move it off the first. A shape with
alternates is marked ×N, and *Assign preset* reads a whole multiple of its length
as pass after pass.

**v0.37.1 "Where You Left It"** — a scene in a folder now saves back to the file
it was opened from. It didn't: the editor kept only the file's NAME and rebuilt
the path from that, so `scenes/cutscenes/Opening.ron` was loaded and
`scenes/Opening.ron` was written — a different file, at the top of the scenes
folder. Reopen the project and the original came back, so every edit looked
undone. Nothing warned, because nothing failed: the save refuses to run during
Play, logs real failures loudly, and only clears the unsaved marker when the
write succeeds — and the write did succeed, to a file the game never reads.
Scenes sitting directly in `scenes/` were never affected; anything in a subfolder
always was. **No work was lost** — it is all in the stray file, and opening a
project now names any scene that has one, without moving anything, because two
files both plausibly wanted is exactly the choice an editor should not make
quietly.

**v0.37.0 "The View That Is The Game"** — a 2D level draws in the Game view. The
editor works out what to draw twice — once for the Scene view, once for every
other view — and the second one had never learned about tilemaps or sprite
batches, so a 2D game was invisible in the one view that IS the game while the
Scene view insisted the level was fine. It has been that way since the 2D layer
arrived. A tileset now carries its own art instead of describing a sheet it did
not draw: point it at an image and it draws, drop images onto the sheets list to
add more, and the grid is worked out from the filename or the pixel size rather
than counted by hand. Nothing had ever uploaded a tileset's sheet to the graphics
card at all — only Material textures — so the extra sheets added last release
have never worked on their own. Setting up an autotile is now clicking a shape
and then the tile that draws it, one rule at a time, each visible as art; it used
to mean selecting every tile in the preset's own order and pressing one button,
with nothing showing what went where. And painting with an autotile no longer
depends on a checkbox elsewhere in the panel, which when unticked silently placed
one fixed corner piece everywhere. Finally: sorting layers. What draws in front
is a named layer with an order inside it, referenced by name so reordering the
list cannot re-sort a scene, and offsetting what is drawn rather than moving
anything.

**v0.36.0 "In Plain Sight"** — animated characters stop flickering. v0.35.0 moved
skinning onto the graphics card and left the depth pass posing the skeleton from
one frame ago, so it decided the shape of a character standing where they had
just been and the pass that draws them put them where they now are; triangles lost
that argument one at a time, differently every frame. Standing still hid it
perfectly. It reached past the characters too — that depth is what the raymarched
scene uses to know when to stop marching, so the floor and walls behind a moving
fighter flashed as well. Also: a flat 2D scene built the obvious way now draws in
the Game view. An orthographic camera sitting in the plane of its own art was
clipping everything at its own depth, so the map showed in the Scene view and the
Game view was empty with nothing said; both views take that depth from the same
place now and cannot disagree again. A tile layer can be cut from more than one
image, so a level made of a ground sheet, a props sheet and a decoration sheet is
ONE layer that still autotiles, merges collision and fills across the joins
instead of three that stop at them — and adding art to one sheet never renumbers a
square you placed from another. Per-tile collision and autotiling were built all
along and simply invisible: the panels that need a tileset returned before drawing
their own headings, so a layer without one showed no collision section and no
autotile section at all. Both are always there now, and say what they are waiting
for. In the image editor the brush outlines the texels it is about to change
rather than a circle it is not — a one-pixel pencil finally shows one pixel — a
tileset's cell grid can be drawn over the canvas and is saved with the image, so
laying out a sheet stops being a job of counting texels, and the checker, pixel
grid and cell grid are all yours to colour from a new View menu. Dragging inside a
selection moves it, Ctrl+J duplicates it without touching the clipboard, and a
transform in flight takes typed numbers. Particle curves say where zero is BEFORE
you cross it, mark the value that means no change, label their extents and stop
rescaling themselves after every edit; the tab reports what an effect actually
costs and draws its particle count along the timeline; and a track asked for more
particles than it can hold now says so instead of quietly making fewer. Curves get
ease in, ease out and hold in one click, and a curve's shape can be reused on
another property at that property's own scale.

**v0.35.0 "A Crowd Of Them"** — animated characters got a lot cheaper. Reshaping a
skinned model used to happen on the processor, one vertex at a time, every frame:
a character of average detail cost about a third of a millisecond, so fifty of
them ate a third of a 60 fps frame before anything drew. That work moved to the
graphics card, and the processor's share of it is now about a thousandth of what
it was. Identical characters also share their geometry again — they could not
before, because two copies of one model would overwrite each other's pose — so
twenty guards of the same model are one draw instead of twenty. Prefabs can now be
opened and edited on their own: double-click one in the Assets panel and its nodes
become the whole viewport — Hierarchy, Inspector, gizmos, undo and Play — with
Save writing back to that prefab file in place, instead of the old route of
dropping it into a scene, editing the copy and using Save as Prefab, which never
overwrote and left a second file beside the one you meant to change. Nothing about
using characters changed: bone attachments, animation events, painted skinned
meshes and pose-hugging selection outlines all behave exactly as they did. And
when a Lua script grows past Lua's 60-variable-per-function ceiling and stops
loading, the engine now names the file, the limit and the fix — it used to print
a line you never edited and leave every script that depended on it silently
empty. A script within ten of the ceiling gets told before it crosses.

**v0.34.0 "Square By Square"** — there is a tile editor now. Press 9 and paint a
level in the Scene view with a brush, a rectangle, a line and a bucket; drag a
block of tiles in the palette and paint the whole block; turn and mirror any tile
or any selection. Tell a tile ONCE that it is solid and every one of them
collides, in every scene, including the ones you already placed — and the
colliders are merged, so a solid floor is one box rather than ten thousand and a
character stops catching on the seams between them. Autotile groups pick their own
corners and edges, with each tile's neighbourhood drawn on the palette so a preset
that guessed your sheet's order wrong is something you can see and fix in a click
rather than something that reads as bad art. Tiles can carry gameplay tags your
game reads off the map, and tiles can animate. There is an orthographic camera at
last — in your game, in render targets and in the editor's own Scene view — so
parallax layers stop changing size as well as speed.

**v0.33.0 "Say So"** — the engine tells you things it used to swallow. Every
options table now refuses a key it does not recognise, naming the property, the
value it got and what it accepts: a typo'd `perchunk` used to take the default
forever, and a typo'd `addative` in `scene.load` **destroyed** the running scene
instead of layering onto it. Four more of that shape are fixed with it. A camera
can be a texture, at a size and refresh rate you pick — minimaps, mirrors,
monitors, scopes and split-screen. There are four accessibility settings a game
can offer (text scale that reflows, a colour-vision filter that corrects rather
than simulates, reduced motion, captions the engine draws). Collision stops
testing every body against every collider, which at 400 bodies over 1,681
colliders is eight times faster. Tab reaches your game at last, a script can
finally export a function called `name`, `-1` clears a tile, nothing behind you is
drawn, and `perf.*` tells your game where its own frame went.

**v0.32.0 "Under Your Feet"** — the ground you are standing on now loads before
a planet twelve thousand units away. Terrain loaded nearest-first, but "nearest"
was counted in each world's own chunks, so on a solar system the ground under
your feet queued behind worlds you could barely see — and while it waited you
could see straight through the planet into space. Both fixed. Scattered props
also stay on the planet they grow on: a field can now ride a node with `parent`,
so a world that orbits carries its rocks instead of sliding out from under them,
and every rock keeps its identity — anything you harvested stays harvested.

**v0.31.0 "Say So Sooner"** before it — a field of scattered props now tells you what it
costs at the moment you ask for it, rather than the moment your game stops. The
outermost view distance was quietly the budget — cost grows with its square —
and `scatter.cost` hands back the numbers, while a field big enough to matter
says so in the Console as you declare it. A big field also degrades instead of
stopping: ground arrives nearest-first over a few frames. Separately, `ui.make`
stops answering typos — a `pin` it doesn't recognise raises and says what it
takes, instead of quietly meaning "top left", and `topCenter`/`bottomCenter` now
work because they are what people write.

**v0.30.0 "Draw It Anywhere"** under those — sprites you draw in `update` now reach the
screen. A sprite batch used to be emptied after every script pass, and a frame
runs three, so anything drawn in `update` was wiped twice before it rendered —
silently, and all of it at once. The frame is the unit now, so draw wherever you
like. A batch's `size` is also the sprite's true width at last (it was drawing
40% too big). Landing on a small planet, terrain that has just arrived dissolves
in instead of popping. And a scene of thousands of scripted nodes no longer
crashes the editor with your unsaved work in it.

**v0.29.0 "Ask The Index"** — one script finding another stopped costing the
size of your scene: a 5,000-node game spent 25 ms a frame searching for things
and now spends 0.2. Worlds can have biomes, because scattered props take a
density rule; a prop your own script assembled can be scattered, because a
prefab works where only a mesh file used to; and landing on a small planet stops
hitching, because detail rings now scale with the world instead of covering it
whole. Plus three silences broken — a scatter option that did nothing, a scene
value your script no longer reads, and a scene value quietly pinning the number
you just edited.

**v0.28.0 "One Missing Section"** — a screen built with `ui.make` no longer
disappears because one part of it isn't showing. A section written as `nil` —
which is how anybody writes a HUD where parts come and go — used to take the
whole screen down, or silently drop everything after it. Clicking also stopped
doing the wrong thing's job: re-describing a screen now removes the handlers it
no longer asks for, so a row that used to be a Buy button and is now a label
stops answering the old one. And a script can make a sprite batch
(`node:setSpriteBatch`) instead of authoring one node per style into the scene.

**v0.27.0 "Same Everywhere"** — a pixel-art game can look the same in every
window size. Two settings, both off by default: upscale the picture by a whole
number and letterbox the rest, so every pixel is the same size instead of some
being two screen pixels and some three; and pin the internal width, so a wide
window stops showing 12% more of your level than a 16:9 one. UI layers get the
same rule, which is what stops a pixel font being resampled off its own grid.
Plus a fix for work a script queued on a session's last frame arriving in the
next one.

**v0.26.0 "Give It Back"** — a busy scene got much faster. Finding what a node
has on it was a search through every node, so a scene's cost grew with the
square of its size; it is now a direct lookup, and a 5,500-node scene went from
60 ms a frame of pure lookups to 4. Nothing in your project changes. Alongside
it: a game can get the mouse cursor back for its own menus — a shop that opens
mid-play used to be unclickable for the rest of the session — and sprites in a
batch can squash and stretch, which is what a 2D game telegraphs an attack with.
The 2D guide now says plainly, with numbers, when a batch stops being optional.

**v0.25.0 "Follow Along"** — Floptle can teach you how to use it. A new
**🎓 Learn** tab holds five follow-along tutorials that build a real game from an
empty project, and because the editor can see what you've made, each step ticks
itself off when you've actually done it — so you never get three steps past a
mistake without noticing. Build a 3D platformer, a top-down RPG, or Flappy; or
start from the finished version of any of them in one click from the New project
screen. There's a fifteen-minute first tutorial for people who have never
programmed, and a twenty-minute orientation for people who have.

The same release adds a **2D layer**: a tilemap that draws a whole level as one
mesh — so the hairline gaps that open between tiles as the camera moves cannot
happen — and sprite batches where every sprite carries its own colour, which is
the difference between flashing an enemy red and blinking it off.

**v0.24.1 "Plain Sight"**: two dozen buttons that had been drawing as
empty boxes got their icons back, and the 188 scripting calls that had no
description anywhere got one — plus a complete reference page and a search that
finds what you typed.

Under those, **v0.24.0 "Say So"** — water you can float on and
freeze, scattered props that cost nothing, and scenes you can layer on top of
each other rather than only swap between.

## Being worked on

**Flying a ship you built.** Compound vessels assemble, dock and come apart
already. What's missing is a flight controller that handles a machine whose
shape you invented — centre-of-mass and centre-of-thrust readouts while you
build, and atmosphere that pushes back on the way up and the way down.

**The arcade fighter.** Fofighter is a real game being built on the engine, and
it's where the netcode gets its hardest test. Nearly every rollback fix in the
v0.10.x releases came from it — which is the point of building a game in your own
engine.

## Working towards

- **Terrain that's really geometry.** Today's terrain is a distance field, which
  is why it has a size cap and why it goes faceted up close. Meshing it removes
  both.
- **Painting straight onto shader graphs**, so a procedural material and a hand-
  painted one stop being separate workflows.
- **Particles on the GPU**, for effects that cost what they look like they should.
- **Collision against shader-defined shapes**, so a surface you wrote in a shader
  is a surface you can stand on.

## Recently

- **v0.22.1 "Front Page"** — a News page in the Hub, a version list you can
  actually click, and every release note ever written rewritten to be about what
  it does for you rather than how it was built.
- **v0.21.2** — the Hub updates itself. One button, and being out of date is
  unmissable.
- **v0.21.1** — function keys reach scripts for the first time, and the Hub shows
  you what a release actually was.
- **v0.21.0 "Who's Playing"** — a game can ask who's sitting in front of it.
  Foverse accounts, cloud saves, leaderboards and missions from Lua.
- **v0.20.0 "Say It Simply"** — the direction and vector helpers that were
  missing, an API browser in the editor, and HTTP from scripts.
- **v0.19.x** — the map knife, blockout painting, undo that takes the paint back,
  and an RTS starter kit.
