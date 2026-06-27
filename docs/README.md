# Floptle documentation

Start here. Floptle is a lightweight, hyperoptimized Rust game engine for surreal,
otherworldly visuals (Fopull LLC). This repo is currently **planning + scaffold** —
the design is written before the engine is built.

## Read in this order

1. [VISION.md](VISION.md) — the north star: the feeling we chase, who it's for, the headline features.
2. [ARCHITECTURE.md](ARCHITECTURE.md) — how the crates and subsystems fit together.
3. [ROADMAP.md](ROADMAP.md) — the phased build plan; each phase ends in a runnable demo.
4. [decisions/](decisions/) — ADRs: every significant choice and *why* (start at [the index](decisions/README.md)).
5. [subsystems/](subsystems/) — deep-dive design per system (start at [the index](subsystems/README.md)).

## The three signature ideas (what makes Floptle unlike other engines)

- **An otherworldly renderer** — SDF raymarching lets you fly *inside* fractals
  that morph in real time, over a post stack that breaks the laws of light.
- **Shaders as graph *and* text** — one custom IR, edited visually or as `.flsl`
  in VSCode (AI-friendly), transpiled to WGSL.
- **Everything is malleable matter** — one implicit-field substrate so any object
  can morph, blend like soup, go soft-body, stick, stretch, and (later) tear —
  and stay cleanly collidable for free.
- **Rules you declare, not mechanics you fake** — light, time, and gravity are
  developer-defined *laws* of a world (a hot-reloadable `lawset.ron`), resolved on
  the same substrate, so a player believes the wall-run because it's what *must*
  happen here — not a trick. ([world-rules.md](subsystems/world-rules.md))

Plus a maker-first toolkit: a video-editor-style particle timeline, in-scene
parametric shape building, automatic object pooling, dead-simple UI, a built-in
dialogue system, and a clean Blender pipeline.

And two foundations that "just work" with zero developer effort: **mass/density
gravity fields** (run on a fractal and up its swirling walls; orbit, land on, and
walk procedural planets) and **large-world space** (the world moves around the
player, so you can simulate a galaxy without precision jitter).
