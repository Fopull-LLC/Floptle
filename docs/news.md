## Just shipped

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

**v0.24.1 "Plain Sight"** before it: two dozen buttons that had been drawing as
empty boxes got their icons back, and the 188 scripting calls that had no
description anywhere got one — plus a complete reference page and a search that
finds what you typed.

The release under both is **v0.24.0 "Say So"** — water you can float on and
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
