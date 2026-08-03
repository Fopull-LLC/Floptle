## Just shipped

**v0.23.0 "Cut Corners"** — your UI can wear a sprite. A panel's edge, a
button's outline, the box around whatever the player has selected: any of them
can be a piece of pixel art you drew, 9-sliced so one small sprite stretches to
any size without smearing its corners. Every frame in a game can live in one
texture, and a frame takes its tint from the style it sits in — so one white
sprite is a bright focused edge and a dim idle one, with the hover transition
already attached.

**v0.22.3 "Which One"** before it, a Hub release about version numbers: the Hub
shows its own version on every tab, and a release that changed only the Hub says
so instead of appearing as a new engine to install.

The engine underneath both is **v0.22.0 "Hold Together"** — modelling that
behaves and levels that survive a scene change.

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
