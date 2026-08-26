# Floptle documentation

Floptle is a lightweight, hyperoptimized Rust game engine for surreal, otherworldly
visuals (Fopull LLC). **Everything that exists is listed on this page.** If it isn't
linked here, it isn't finished — a test enforces that.

> **New here?** [getting-started.md](getting-started.md) installs the engine and gets
> something on screen; [ARCHITECTURE.md](ARCHITECTURE.md) is how the pieces fit together.

## Find it fast

| I want to… | Go to |
| --- | --- |
| **Learn by making a small game, step by step** | [tutorials/](tutorials/README.md) — or the editor's **🎓 Learn** tab |
| Get something on screen and walk around in it | [getting-started.md](getting-started.md) |
| Look up what a Lua call does | [lua-api.md](lua-api.md) — every name, or the editor's **§ Docs** page |
| Learn scripting properly, in order | [scripting.md](scripting.md) |
| Make things fall, collide, be stood on | [physics.md](physics.md) |
| Build a level without leaving the engine | [map-tools.md](map-tools.md) |
| Make a **2D** game — tiles and sprites | [2d.md](2d.md) |
| Paint a tile level: palettes, autotiling, collision | [tilemaps.md](tilemaps.md) |
| Add a minimap, mirror, scope or split-screen | [render-targets.md](render-targets.md) |
| Make your game playable by more people | [accessibility.md](accessibility.md) |
| Sculpt terrain / build a planet | [subsystems/deformable-matter.md](subsystems/deformable-matter.md) |
| Draw a texture | [image-editor.md](image-editor.md) |
| Build a menu or a HUD | [ui-tab.md](ui-tab.md) → [ui-styles.md](ui-styles.md) → [ui-make.md](ui-make.md) |
| Animate a character | [animation.md](animation.md) |
| Add sound | [subsystems/audio.md](subsystems/audio.md) |
| Make particles / VFX | [subsystems/particles-vfx.md](subsystems/particles-vfx.md) |
| Write a shader | [subsystems/shaders.md](subsystems/shaders.md) |
| Go multiplayer | [multiplayer.md](multiplayer.md) |
| Ship a build | [export-builds.md](export-builds.md) |
| Talk to a website / sell something | [web-api.md](web-api.md) |
| Install a package, or write one to share | [packages.md](packages.md) |
| Add my own tools to the editor | [editor-scripting.md](editor-scripting.md) |
| Understand *why* it's built this way | [VISION.md](VISION.md) |
| Light a 2D scene | [2d.md](2d.md#2d-lighting) |

## Learning it

- [tutorials/](tutorials/README.md) — **follow-along projects**: a 3D platformer, a
  top-down RPG, Flappy, plus a first-hour introduction and a twenty-minute orientation
  for people who already program. The same steps are in the editor's **🎓 Learn** tab,
  where each one ticks itself off as your project comes to match it, and three of them
  ship as starter templates you can create straight from the Hub.

## Using the engine

The build-something guides. Each one is a path from nothing to a working result.

- [getting-started.md](getting-started.md) — from empty project to a **walkable
  first-person scene**: sculpt terrain, add a player, gravity, mesh colliders, scripts.
- [scripting.md](scripting.md) — the **Lua guide**, taught in order: the `node` transform
  and physics body, `input`, `raycast`, lifecycle, UI, netcode, scenes, water, scatter.
  Mirrored in-engine on the **Scripting ▸ § Docs** page.
- [lua-api.md](lua-api.md) — the **complete Lua reference**: every name a script can
  reach, grouped and searchable. Generated from the same table that drives the editor's
  Docs tab, its hover docs and its autocomplete, so all four always agree.
- [physics.md](physics.md) — **rigidbodies, gravity, colliders, raycasting** and how the
  play loop runs.
- [animation.md](animation.md) — **clips, controllers, layers and blending**: baked
  `.anim.ron` files, glTF import, and driving it all from Lua.
- [map-tools.md](map-tools.md) — the **▦ Map** tool: draw, cut, texture and
  **paint** blockout levels in the editor, without a round trip to Blender.
- [image-editor.md](image-editor.md) — the **🖼 Image** tab: draw a texture in the
  engine and watch it change on the mesh (pixels, paint and vectors, one document).
- [2d.md](2d.md) — **tilemaps and sprite batches**: a level as one seamless mesh, and
  many sprites from one node with a tint each.
- [tilemaps.md](tilemaps.md) — the **◫ Tiles** suite: paint a level with brush,
  rectangle, line and bucket; a palette that is also the tileset editor (per-tile
  collision, tags and animation); autotile groups that pick their own corners;
  merged tile colliders; and the orthographic camera.
- [render-targets.md](render-targets.md) — **a camera as a texture**: point a camera at
  a name and wear its picture on a material or a UI image — minimaps, mirrors, security
  monitors, scopes and split-screen, each at its own size and refresh rate.
- [accessibility.md](accessibility.md) — **text scale, colour-vision filters, reduced
  motion and captions**: four settings a game exposes and the engine honours, plus
  what only your game can do (your camera shake reads the flag).
- [multiplayer.md](multiplayer.md) — from a single-player scene to **two machines playing
  together**: which replication mode to pick, prediction, rollback for fighting games,
  testing on one desk, and shipping (relay, dedicated server, interest management).
- [web-api.md](web-api.md) — talking to a **website or your own server**: the account
  flow, missions, and Fobucks.
- [packages.md](packages.md) — **packages**: modular expansions anybody can write and
  share — editor tools, scripts, art — installed from a folder, a repository or the
  browser, and how to write and publish one of your own.
- [editor-scripting.md](editor-scripting.md) — the **editor API** a package gets: menus,
  panels, Scene-view overlays, world-space handles, scene edits with undo, preferences,
  and talking to a server.
- [export-builds.md](export-builds.md) — **shipping a build** players can run.
- [updating-the-hub.md](updating-the-hub.md) — the one **manual Hub update**; from
  v0.21.2 onward the Hub updates itself.

### Building UI

- [ui-tab.md](ui-tab.md) — the **◫ UI** tab: the authoring canvas where screens get built.
- [ui-styles.md](ui-styles.md) — **styles and tokens**: how to stop typing colours onto
  elements one at a time.
- [ui-navigation.md](ui-navigation.md) — **keyboard and gamepad focus**, for a menu that
  isn't mouse-only.
- [ui-make.md](ui-make.md) — **screens from data** (`ui.make`): a roster of four fighters
  or nine, an inventory of whatever the player is carrying.
- [ui-demo.md](ui-demo.md) — the demo scene that uses all of it at once. Open it and press
  Play.

## Design docs (how it's built and why)

1. [VISION.md](VISION.md) — the north star: the feeling we chase, who it's for, the headline features.
2. [ARCHITECTURE.md](ARCHITECTURE.md) — how the crates and subsystems fit together.
3. [subsystems/](subsystems/) — deep-dive design per system (start at [the index](subsystems/README.md)).

## Releases

- [releases/](releases/) — the notes for every version. These are what a player reads.
- [news.md](news.md) — what's on the Hub's 📰 News tab right now.

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
