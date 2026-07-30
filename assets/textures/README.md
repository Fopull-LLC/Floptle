# assets/textures

Built-in/default textures that ship with Floptle live here — this is the sample
project, so everything below is here to be used, edited, or thrown away.

## What's in here

| Folder | What it is |
|---|---|
| `Materials/` | 518 seamless 256×256 surface textures — brick, wood, stone, metal, tile, cloth, terrain, gems, gratings… (CC0) |
| `VFXTEX/` | 197 soft particle sprites — fire, smoke, shockwaves, slashes, lightning, debris |
| `VFXPX/` | 74 **pixel-art** particle sprites: the same vocabulary at retro resolution |
| `UI/` | UI art for the shipped demo screens |
| loose files | odds and ends used by the example scenes |

### `Materials/` — three packs, kept apart on purpose

- **Base Materials** — plaster, stone, clay, wood, cloth, cardboard, marble,
  concrete. Names are already unique (`Mat_Stone_Black_01`).
- **Tiny Textures 2** — brick, dirt, elements, metal, plaster, stone, tile, wood.
- **Tiny Textures 3** — animal, box, cloth, elements, gem, grating, metal, stone,
  terrain, weave.

Packs 2 and 3 both contain a `Stone_01.png` and they are *different images* —
which is why the three stay in separate folders instead of being merged by
category. Everything is 256×256 and tiles seamlessly.

All three are from [Screaming Brain Studios](https://screamingbrainstudios.itch.io/),
released **CC0 / public domain**: use them in anything, commercial or not, with
or without credit. Each pack keeps its own `License.txt`, which also carries the
author's patron credits — worth leaving in place.

### Using one

Set it on a node's material from a script:

```lua
node:setMaterial{ texture = "textures/Materials/Tiny Textures 2/Brick/Brick_06.png" }
```

In the editor: **Inspector ⏵ ◆ Material ⏵ texture**. Tiling sits next to it —
`Uv` for flat surfaces, `Triplanar` for terrain and anything irregular — so one
256×256 tile covers a whole wall without stretching.

The particle sets are for the **❋ Particles** tab: a track's texture takes any
of them. Reach for `VFXPX` in a retro-resolution scene — soft sprites turn to
mush when the whole frame is 320 px wide.

## Temporary test assets (Ocarina of Time)

During development we use Ocarina of Time textures **only** as local placeholders.
Place them under `assets/textures/_oot_temp/` — that folder is **git-ignored**
on purpose so the copyrighted files never enter version history (we plan to
open-source later, and scrubbing history is painful).

Before any public release these must be replaced with original Fopull textures.
See `docs/decisions/0010-temporary-assets.md`.
