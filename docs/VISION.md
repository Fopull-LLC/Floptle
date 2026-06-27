# Floptle — Vision

> The north star. Every decision in this repo should trace back to something here.
> If a feature doesn't serve this vision, it doesn't go in.

## 1. The feeling we're chasing

Most engines optimize for *fidelity* — making a thing look like a real thing.
Floptle optimizes for the reaction:

> "I've literally never seen anything that looks like this. It looks like it's
> from another dimension."

Surreal. Otherworldly. Trippy. Complex. And, a few layers down, quietly
**nostalgic and dreamlike**. We are willing to break conventional rendering
norms — and the "laws" of light and geometry themselves — to get there. Realism
is not a goal; *novelty of perception* is.

This is a creative instrument first and a technical artifact second. But to be a
great instrument it must also be fast, lightweight, and a pleasure to use.

## 2. Who it's for

A solo (or tiny-team) maker who:

- builds **surreal, dreamlike adventure games** and **flashy, fast combat games**,
- models in **Blender** and wants those models in-game with no fuss,
- has **VFX instincts** and is tired of clunky particle tools,
- wants to write shaders by graph *and* by text (with AI help),
- self-hosts, and may one day want **networked** games on their own infra,
- and above all wants to **make freely, without other people's restrictions**.

The first such maker is Ty / Fopull LLC. The engine should feel tailor-made.

## 3. Non-goals (so we stay lightweight)

We deliberately **do not** chase:

- Photorealism, film-grade PBR, or AAA-scale rendering features.
- Unreal-tier animation (complex IK rigs, motion matching, mocap pipelines).
- A giant property-soup UI for every subsystem.
- Being everything for everyone. Opinionated > configurable-to-death.

When in doubt: *the simplest thing that gives the maker real creative control.*

## 4. What makes Floptle different (the headline features)

### 4.1 An otherworldly renderer
A custom render graph whose default toolbox is **signed-distance fields and
raymarching**, not just triangle rasterization. Fractals are math, so we render
them as math — letting you **fly inside a fractal** and watch its geometry and
patterns morph into each other in real time. Layered with screen-space passes
that intentionally violate normal lighting (impossible color transport, melting
space, feedback trails) to produce the "from another dimension" look.

### 4.2 Shaders as graph *and* text — our own shading language
One shader is one source of truth, the **Floptle shader IR**. Edit it as a node
graph in the editor; press a button and the *same shader* opens as readable text
(`.flsl`) in VSCode for AI-assisted authoring; switch back freely. The IR
transpiles to WGSL. Owning the shader representation is our biggest lever for
visuals nobody else can make.

### 4.3 A VFX editor that works like a video editor
Name an effect → set its lifetime → choose looping vs. one-shot → a **timeline**
appears. Drop **particle groups** on it (e.g. "Crescents", "Smoke"), each with
its own behavior. Every property is a constant *or* a **curve over lifetime**
(hover a corner, a graph icon appears, draw the curve). Emission is either an
auto rate ("once every 0.2s" → auto `Emit` events on the timeline) or
hand-placed `Emit` events you scatter wherever you want. This is the whole flow,
in-engine, designed by someone who's actually done VFX work.

### 4.4 Made-for-makers everywhere else
- **Scene view** you build *in*: right-click → Create → Shape → draw a base on
  the ground, pull it up to set height; procedurally generated Cube/Sphere/
  Cylinder/Capsule/Wedge/Stairs (stairs has a "step count" property), all
  editable after creation, collidable toggle, material + texture per shape.
- **Textures that just tile**: drag a texture onto a surface, say "repeat 3×,
  flip on alternate" — no shader required.
- **Dead-simple UI**: arrange elements, anchor them, script what they do.
- **Built-in dialogue**: typewriter text with per-character voice SFX, skip-to-
  full, advance-on-interact, woven into cutscenes — themeable, good defaults.
- **Cameras**: first-person, third-person, and clean gameplay↔cutscene blends.
- **Automatic object pooling**: take/return from a pool; no manual setup.
- **Input**: bind any key/mouse/gamepad input(s) to named actions; script them.
- **Nodes + components/scripts**: attach a script, click it, it opens in VSCode
  with the project as the workspace root and that script focused.

### 4.5 Everything is malleable matter (the unifying idea)
Nothing in Floptle is forced to be a static box. Any object — a fractal *or* a
shape you built — can be told how it behaves **as physical matter**: its vertices
can morph and shift in real time while staying cleanly collidable; two pieces of
geometry can **blend together like soup**, **mix**, or **reject** each other;
objects can be given **soft-body** behavior, made to **stick** and physically
**stretch** when pulled, and (later) **tear into stringy strands and split apart**.

The trick that makes this both possible and fast: all geometry shares one
**implicit-field substrate** (signed distance functions), so combining shapes is
just math (smooth-min/-max with a blend radius), and *the same field the renderer
draws is the field physics collides against* — a deformed object is automatically
renderable and collidable, no desync. Complexity is opt-in via tiers (Rigid →
Morph → Field-blend → Soft-body → Viscoelastic): you only pay for the behavior you
reach for. This is the engine's most distinctive underlying system. See
[`subsystems/deformable-matter.md`](subsystems/deformable-matter.md) and ADR-0013.

And because matter has **density**, it has **mass**, and mass **emits gravity** —
so gravity in Floptle is a *field* `g(p)`, not a global constant. You can run
around on a fractal and **up its swirling walls**, kept grounded by the field the
surface emits (not floaty anti-gravity); orbit and then *land and walk on*
procedural fractal **planets**; or fly a ship between them. Density also decides
whether matter **crushes** under pressure (soft clay) or **resists** (hard metal).
The engine understands space, matter, gravity, and density as one idea. See
[`subsystems/gravity-and-density.md`](subsystems/gravity-and-density.md) and ADR-0014.

## 5. Constraints & platform

- Ships for **Linux, Windows, macOS** from one codebase.
- **Lightweight & fast** is a feature, not an afterthought — measured, not hoped.
- **Large worlds by default**: the world moves around the player (floating-origin /
  camera-relative space), so you can simulate a whole **galaxy** and travel
  absurdly far with no precision jitter — automatic, zero developer work. See
  [`subsystems/large-world-space.md`](subsystems/large-world-space.md) and ADR-0015.
- **Networking** is explicitly *future* (dedicated server build + clients on the
  maker's own infra), but the architecture must not preclude it.

## 6. Identity & look-and-feel of the tool itself

The editor is **dark-themed, somewhat high-contrast, retro / pixel-art inspired**
— yet organized, readable, and clear. High user customizability and control.
The tool should feel like it belongs to the same universe as the games it makes.

## 7. Definition of "ready to exist as a product"

Floptle becomes a (free, open-source, donation-supported) Fopull LLC product when:

1. A maker can build a small surreal game **and** a small combat game end-to-end
   using only the features above.
2. It exports running builds for all three platforms.
3. All temporary Ocarina-of-Time test assets are replaced with original Fopull
   art (see ADR-0010).
4. The headline features (renderer, shader IR, VFX timeline, scene-building,
   UI, dialogue, pooling) feel *better* to use than the incumbents — that's the
   whole point.

---

*See [`ROADMAP.md`](ROADMAP.md) for how we get there in phases, and
[`ARCHITECTURE.md`](ARCHITECTURE.md) for how the pieces fit.*
