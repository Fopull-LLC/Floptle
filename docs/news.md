## Just shipped

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
