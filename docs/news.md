## Just shipped

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
