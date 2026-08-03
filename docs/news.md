## Just shipped

**v0.22.1 "Front Page"** is a Hub release — this page is new, the version list
is a list you can actually click, and every release note ever written has been
rewritten to be about what it does for you rather than how it was built.

The engine underneath it is **v0.22.0 "Hold Together"** — modelling that behaves
and levels that survive a scene change.

Loading one scene from another used to bring the level in stripped: untextured
surfaces, wrong materials, missing paint and collision that didn't match what you
could see. Opening the same scene directly looked perfect, which made it look
like your scene file was broken. It's fixed — a level loads the same way
whichever direction you arrive from.

The map tool stopped folding. Faces with more than three corners were being split
into triangles the simplest possible way, which is only correct while a face stays
flat and convex — and moving any vertex ends both. Dragging a corner could crease
a face, stretch it, or spill it outside its own outline. Faces are split properly
now, so what you select and what you walk into match what you see.

Also in it: **right-click in the viewport** for everything you can do to a
selection, **Select ⏵ Warped faces** to find the one that's wrong, **disable a
node** and everything under it, shortcuts that stop getting stuck after you
alt-tab, a scene tree that opens folded, and a crash handler that offers to file
a report for you with the details already filled in.

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
