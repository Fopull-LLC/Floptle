# The Image editor — one canvas for pixels, paint and vectors

> **Draw on the left, watch the mesh change on the right, never save-and-alt-tab.**

Every texture in a game used to be made somewhere else: you leave the engine,
draw in Aseprite or Krita, save, alt-tab back, and find out whether it looked
right on the model. The **🖼 Image** tab deletes that round trip, because the
engine is the only program that can show you the texture on the thing it is for,
lit the way it will be lit, while you are still holding the brush.

Design rationale and the decisions behind it:
[image-editor-proposal.md](image-editor-proposal.md).

---

## 1. Two minutes to your first texture

1. Open the **🖼 Image** tab (it ships in the same dock group as ⌖ Scene, or
   **Window ▸ Reset layout** brings it back).
2. **New image…** → pick a preset (16²–128² are pixel-art sizes; 512²+ default to
   painterly) → **Create**.
3. Draw. `B` is the pencil, `E` the eraser, `G` fills, `[` and `]` change the
   brush size.
4. **Save** (or `Ctrl+S`) → give it a name → it lands in your project's
   `textures/` folder as **two** files:
   - `name.flimg` — the layered document. This is what you re-open and keep editing.
   - `name.png` — the flattened image. **This is the one scenes and materials use.**
5. Assign that `.png` to a material like any other texture.
6. Tick **Live** in the toolbar, split the **⌖ Scene** tab beside this one, and
   keep drawing. The mesh updates as you paint.

### The PNG contract

The document is the source of truth; the PNG is a build artifact. Every save
rewrites the PNG beside the document, so:

- Nothing else in the engine changes — scenes, materials, the asset browser and
  the exporter all keep pointing at PNGs and never learn documents exist.
- A project with the `.flimg` files deleted still builds and ships.
- The runtime never reads `.flimg`. Deliberately: it is a load-bearing
  simplification, not an oversight.
- Double-clicking any image in **Assets** opens it here. If a sibling `.flimg`
  exists you get the layered document; if not, the image is wrapped in a
  one-layer document you can save whenever you like.

---

## 2. Live reload (and why it also helps Aseprite users)

The texture registry used to cache a GPU upload by path and never look at the
file again — rewriting a PNG on disk changed nothing on screen until the project
was reloaded. Both halves of the fix ship here:

- **Push** — the editor invalidates a texture the instant it writes it, so the
  scene view updates on the same frame as the save.
- **Poll** — an mtime check over textures that are actually resident, twice a
  second. That means **external tools hot-reload too**: save in Aseprite, Krita
  or Photoshop and Floptle picks it up with no import step. If you'd rather keep
  your existing tools, you still get the fastest half of this feature.

**Live** takes it further: with it on, each quiet moment between strokes
re-exports the PNG (never mid-stroke, at most four times a second, PNG only —
the layered document still waits for `Ctrl+S`).

---

## 3. Three ways of working, one editor

Mode is a *preference that sets defaults*, not a fork in the road. Any document
can hold any mix of layer kinds; switch mode whenever you like.

| | **Pixel** | **Painterly** | **Vector** |
|---|---|---|---|
| Anti-aliasing | off | on | on |
| Zoom | integer factors, nearest | continuous | continuous |
| Grid | on above 6× | off | off |
| Brush default | 1 px pixel-perfect pencil | soft, 12 px | — |
| Export sampling | `Pixelated` | `SmoothMipmaps` | either |

In Pixel mode the zoom snaps to integer factors and the pan snaps to whole
texels, so one image pixel is always an integer number of screen pixels. A pixel
editor that shows a blurry approximation of your own art is worthless.

---

## 4. The canvas

| Gesture | Does |
|---|---|
| Wheel | Zoom at the cursor |
| `Ctrl`+wheel | Brush size |
| Middle-drag, or `Space`+drag | Pan |
| Right-drag | Eyedropper (picks continuously) |
| `0` / `Ctrl+0` | Zoom to fit / 100 % |
| `Shift` while dragging a shape | Constrain (squares, 45° lines) |
| `Alt`+click (clone stamp) | Set the clone source |

Nothing moves on its own: the view is never re-centred or re-fitted except when
you ask. Opening a document is the single exception, because there is no previous
view to preserve.

### What the cursor tells you

The brush telegraph outlines **the texels the brush is actually going to
change**, on the pixel grid, taken from the brush's own footprint. A one-pixel
pencil shows one square texel under the cursor — not a small circle floating
between them. A soft brush shows two contours: the solid one is where the brush
is, the faint one is how far it reaches, so a soft brush never claims a hard edge
and a hard one is never given a halo it does not have.

Very large brushes and very low zooms fall back to a plain circle: at that size
there is nothing to learn from the exact texels.

### View ▸ the overlays

Everything drawn over your art is yours to set, because whether an overlay is
visible depends on art nobody can predict.

| Setting | Notes |
|---|---|
| Transparency checker | Both colours and the square size (in **screen** pixels, so it looks the same at any zoom) |
| Pixel grid | Colour, opacity, and the zoom it starts appearing at |
| Pixel grid ▸ Two-tone | Dark dashes over the light line, so one of the two shows against art of any colour. On by default — legible without being configured for it |
| Sheet cell grid | Colour and opacity; see §9 |

**Reset overlays to defaults** puts them all back. These are per-user settings,
saved beside your other preferences — how you like to look at images, rather than
a fact about one image.

### Tools

Letter keys, live only while this tab has focus (the viewport's tool digits are
untouched).

| Key | Tool | | Key | Tool |
|---|---|---|---|---|
| `B` | Pencil ⇄ Brush (toggles) | | `M` | Select box (`Shift+M` ellipse) |
| `E` | Eraser | | `Q` | Lasso |
| `G` | Fill (`Shift+G` gradient) | | `W` | Magic wand |
| `L` | Line | | `V` | Move layer |
| `U` | Rectangle (`Shift+U` ellipse) | | `I` | Eyedropper |
| `A` | Reshape (vector) | | `P` | Pen (vector) |
| `T` | Text | | `Ctrl+T` | Free transform |
| `X` | Swap colours | | `[` `]` | Brush size |
| `Delete` | Erase the selection | | `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| `Ctrl+C`/`X`/`V` | Copy / cut / paste | | `Ctrl+A` / `Ctrl+D` | Clear the selection |
| Arrows | Nudge 1 px (`Shift` = 10) | | `+` / `-` | Zoom in / out |

The whole list is also in the editor: **Edit ▸ ? Keyboard shortcuts**. Every tool
icon in the strip is *drawn* rather than typed, because the bundled font stack has
no pencil, brush, eraser or pen glyph and a missing glyph ships as a blank square.

**Copy / cut / paste** work on the selection — or, with none, everything painted
on the layer. A paste arrives as a **floating block** under the cursor with the
transform handles already on it: drag it where it goes, `Enter` applies, and one
`Ctrl+Z` takes the whole paste back. It never clears what it lands on, and the
clipboard survives closing one document and opening another.

`Ctrl+Z` here is **this tab's own** undo stack. It never touches the scene's —
image edits are not scene edits, and a scene snapshot per brush stroke would be
absurd.

**Free transform** (`Ctrl+T`) lifts the selection — or, with no selection,
everything painted on the layer — into a box you can drag, scale from any corner
(`Shift` = uniform) and rotate from the handle above it (`Shift` = 15° steps),
with numeric fields in the panel for the exact values. `Enter` applies, `Esc`
cancels exactly, and the whole thing is one undo step.

**Text** (`T`) places a block, types into it live, and rasterizes through the
editor's own font atlas — so text stamped into an image matches the text beside
it in the UI. In Pixel mode the coverage is hard-thresholded so it stays crisp.
`Ctrl+Enter` applies, `Escape` cancels and restores the document byte-for-byte.

A floating transform or text block belongs to the layer and frame it was made on,
so switching either **settles it first** rather than stamping it somewhere it was
never aimed at. Undo cancels one outright instead of re-applying it over restored
pixels.

The brush has eight modes beyond plain paint: **erase, smudge, blur, sharpen,
dodge, burn and clone stamp**, each sharing one radius / hardness / flow /
spacing / blend profile.

### Selections

Box, ellipse, lasso and magic wand, combined with **replace / add / subtract /
intersect**, plus feather, grow, shrink and invert. A selection clips *every*
subsequent operation — brush, fill, gradient, filter, adjustment, delete — which
is the difference between a toy and a real editor. It's 8-bit, so a feathered
lasso and a hard marquee are the same object.

**Select ▸ Use as layer mask** turns a selection into the active layer's mask,
which you can then paint on directly (switch the brush's *paint into* row to
**mask**). **Image ▸ Crop to selection** cuts the canvas down to what's selected.

Flips and rotations turn the *whole document* — pixels, layer masks, the live
selection and any vector paths together. A canvas op that moved the pixels and
quietly dropped the masks would be the worst kind of correct.

---

## 5. Layers

Add pixel, vector and adjustment layers; reorder, rename, hide, lock, set opacity
and one of **18 blend modes** — the same six the 3D brush speaks (Mix, Multiply,
Add, Subtract, Lighten, Darken) plus the rest of the standard vocabulary. They
are defined once, in `floptle-image`, so a 2D layer and a 3D dab set to
"Multiply" can never drift into meaning different things.

- **Clip to layer below** confines a layer to the alpha of the one under it.
- **Masks** are per-layer, paintable, and toggleable.
- **Adjustment layers** re-evaluate forever: Levels, Curves, Hue/Saturation,
  Brightness/Contrast, Colour balance, Posterize, Threshold, **Palette quantize**,
  Gradient map, Invert, Desaturate.
- **Layer effects** are non-destructive: Outline, Drop shadow, Glow (inner or
  outer), Colour overlay. Outline is the load-bearing one — a sprite that reads
  against any background needs one, and doing it by hand in pixel art is misery.
Each row carries a live thumbnail (rebuilt on a slow rate limit, never while
you're drawing), so which layer is which is a glance rather than a read.

- **Merge down** and **Flatten** bake everything correctly (blend, opacity, mask
  and effects included). Flatten keeps every animation frame.

**Filters** are destructive and explicit, and every one has a **live preview**:
Blur, Sharpen, Noise, Pixelate, Offset (wrap), Make seamless, Normal map from
height. Drag the sliders, watch the canvas, then Apply or Cancel.

---

## 6. Tiling textures

Three separate things, and you want all three:

1. **Tiling mode** (Image ▸ Tiling mode) — strokes wrap at the canvas edges. A
   stroke that leaves the right edge enters at the left. This is what actually
   *makes* a texture seamless; the rest just tells you whether you succeeded.
2. **Show 3×3 repeat** — draws the canvas nine times so you see the repeat while
   you work. The centre tile is the editable one.
3. **Filter ▸ Seam finder** — rolls the image half a canvas so the seams land in
   the middle where you can paint them out. **Filter ▸ Make seamless** does the
   mechanical version by mirror-blending both edge bands.

Blur and normal-map generation also read across the seam when the document is
tiling, so they can't build a bright rim exactly where the texture repeats.

---

## 7. Palettes

Pixel art lives or dies on a constrained palette.

- Built-ins ship with the editor (Sweetie 16, PICO-8, Game Boy, Endesga 8).
- Drop `.gpl` (GIMP/Aseprite/Lospec) or `.hex` (Lospec) files into
  `.floptle/palettes/` and they appear in the ▾ menu. Format is detected by
  content, not extension.
- **From this image** builds a palette out of what you've already drawn.
- **Lock** snaps every colour you place — primary *and* secondary — to the
  nearest entry, so a stroke can't introduce an off-palette colour by accident.
- **Palette quantize** (as an adjustment layer) reduces *anything* — a photo, a
  painted texture — to N colours with optional ordered or diffusion dithering.
  This is how a painted texture becomes retro art, and it's why the painterly and
  pixel halves of the tool feed each other rather than sit in separate rooms.

---

## 8. Vectors

Deliberately Scratch-shaped, because that model fits a game developer far better
than pen-tool handle etiquette:

- **Reshape (`A`)** — click a path, drag a node, the shape follows.
- **Double-click a node** to toggle corner ↔ curve.
- **Click an edge** to insert a node.
- Handles exist and are draggable (for the selected node), and `Shift` breaks
  their symmetry — but a curve node with no handles derives smooth ones from its
  neighbours, so you can draw a rounded blob without knowing what a handle is.
- **Pen (`P`)** lays down nodes; click the first node (or `Enter`) to close.
- Fill (solid or gradient) and stroke (width, cap, join) per path.
- The shape tools (`L` `U`) have an **as a vector layer** checkbox: same tool,
  one modifier, re-editable shapes instead of pixels.

Vectors stay vectors in the file and rasterize at composite and export, so a logo
re-exports crisply at any size.

---

## 9. Frames and sprite sheets

The engine already consumes sprite sheets in two places — UI images address a
`cell` (animatable in the dopesheet) and VFX billboards flipbook through one — so
frames are wired straight into that.

- **✚** in the Frames panel adds a frame (and makes the layers per-frame).
- Per-layer **per-frame** toggle: a background layer can stay static while the
  character animates.
- **Onion skin** ghosts the previous frame under the current one.
- Playback at the document's fps.
- **File ▸ Export ▸ Frames → sprite sheet** packs one **uniform, row-major grid**
  and writes `cols`/`rows` into `.floptle/textures.ron` — so the sheet the packer
  makes is exactly the sheet the runtime can address. No manual counting.
- **Frames → animated GIF** for sharing.

### Drawing a tileset: the cell grid

A sheet is a uniform grid of cells, and until you can see that grid you are
counting texels by hand to find where tile 3 starts.

**View ▸ This image is a sheet** turns the grid on and saves it *with the image*,
because how the art is cut is a fact about the art: close the file, reopen it,
the grid is still there. Set it as **cols × rows**, or click a **cell size**
(8, 16, 24, 32, 48, 64) and the counts follow.

A grid that does not divide the canvas evenly draws nothing and says so — a
10.6-pixel cell is a mistake to draw against, not a number to round.

The cell grid is deliberately not a heavier pixel grid: it is a different colour,
about a different unit, and it draws over the pixel grid where the two coincide.

---

## 10. Where things live

```
crates/floptle-image/          the kernel: no egui, no wgpu, ordinary cargo test
  doc.rs  tiles.rs  composite.rs  blend.rs  brush.rs  select.rs
  adjust.rs  effect.rs  filter.rs  vector.rs  palette.rs  sheet.rs
  transform.rs  io.rs
  examples/flimg_probe.rs      renders PNGs you can look at (the visual harness)
                               — the compositor, the path rasterizer, tiling

crates/floptle-editor/src/
  image_edit.rs   canvas: view transform, tool state machine, overlays, undo
  image_icons.rs  the tool icons, DRAWN (the font stack has no pencil or brush);
                  `every_icon_has_ink_in_its_cell` tessellates them and rasterizes
                  the triangles through the kernel's own filler, so the strip can
                  be LOOKED at without a window — and fails if a cell comes out
                  blank
  image_ui.rs     the 🖼 tab: menus, tool strip, layers, palette, frames
  image_io.rs     project glue: open/save/export, texture invalidation, hot reload
  icons.rs        `every_glyph_in_the_image_tab_renders` scans this tab's string
                  literals and refuses any character the font stack can't draw
```

Pixels live in 128² copy-on-write tiles, allocated on write: a 4096² document
with a corner painted costs the corner, a stroke dirties a handful of tiles, and
an undo entry is a document clone that copies refcounts rather than pixels. The
status bar reports what's actually resident.

---

## 11. Not in this version

Stated plainly so nobody hunts for them:

- **Layer groups** — the stack is flat (clipping covers most of what groups are
  used for).
- **Rich text** — the text tool is one font, one size, one colour per block (the
  editor's proportional face). No font picker, no per-character styling.
- **Texture paint v2** — baking a mesh's 3D paint into a document, and painting
  into a document from the viewport (proposal §13).
- **Procedural layers** from the ◈ shader graph, and the GPU compositor
  (proposal §9, §12) — the CPU path is the reference implementation either way.
- **An `image.*` Lua API** — deliberately none; the model should settle first.
