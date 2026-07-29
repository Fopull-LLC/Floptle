# The ◫ UI tab

Where flat screens get built.

This covers the UI authoring canvas. For the element vocabulary — gradients,
9-slice, text effects — see the Inspector's **UI Element** section; for styles
and tokens see [ui-styles.md](ui-styles.md); for the design rationale see
[ui-system-2-proposal.md](ui-system-2-proposal.md).

**The tab is optional and imposes nothing.** Every edit it makes is an ordinary
component change you could have typed into the `.ron` by hand. It ships no
theme, no widget kit and no default look; where it needs a number it doesn't
have — a snap step — it takes it from your project's own tokens.

---

## What's on screen

The canvas is **the real renderer**. It's the shipping UI pipeline drawing your
layer into an offscreen target, so a gradient, a `stage ui` shader, a 9-slice
border and a blend mode all look here exactly as they look in the game. Nothing
is approximated for the editor.

Everything else — outlines, handles, guides, the insertion caret — is chrome
drawn on top.

| | |
|---|---|
| **Layer** | which screen you're building. A scene can hold several. |
| **Resolution** | the resolution to *solve* at (see below). |
| **Zoom** | `1:1` is one design unit per pixel at the reference resolution; `Fit` frames the whole canvas. Zoom re-renders, so text stays crisp — it never scales an image up. |
| **Backdrop** | the colour behind the layer. Set it to whatever your game actually shows behind this screen. |

Navigation is the same as every other canvas in the engine: **wheel** zooms
about the cursor, **middle-drag** pans, **drag on empty space** rubber-band
selects, **shift/ctrl-click** adds to the selection.

The canvas never re-frames itself. `Fit` happens when you ask for it and when
you switch layers — never while you're working.

---

## The resolution dropdown

This is the most useful control in the tab and the least obvious.

Picking a resolution doesn't stretch the picture — it **re-solves the layer** at
that shape, through the layer's own canvas scaler. So switching from 16:9 to
21:9 shows you the truth:

- a `Free` element doesn't move (that's what Free means),
- a `Pin` element tracks its corner,
- a `Stretch` element grows into the new width.

When the preview shape differs from the layer's reference resolution, the
reference box is outlined on the canvas. The area outside it is what an
ultrawide gives you for free — or, on an `Expand` layer, what gets letterboxed
away.

If a layout only ever gets looked at at one resolution, `Pin` and `Stretch` are
just extra typing. This is the control that makes them worth using.

---

## Snapping

Three sources, in priority order:

1. **Guides** you placed.
2. **Sibling edges and centres**, plus the containing element's box.
3. **The grid** — the fallback.

An explicit guide or a real edge always beats "a multiple of 8", because you put
it there on purpose.

The element snaps by **whichever of its own edges is nearest** — leading edge,
centre, or trailing edge — so a panel lines up with its neighbour by the edge
you're actually looking at. When a snap catches, the line it caught on is drawn.

**The grid step defaults to your project's smallest spacing token.** If your
tokens say `xs: 5`, the easy drag lands on multiples of 5. The engine has no
opinion about what your spacing scale should be; it only insists that dragging
should land on it. With no tokens defined, or with a number typed into the grid
field, that override wins.

`🧲` toggles the whole thing off.

### Guides

Drag off the top or left ruler to make one. Drag it back onto the ruler (or
clean off the canvas) to delete it. Guides are stored per scene, keyed by layer
*name*, under `.floptle/guides/` — authoring data that can never change what
ships.

---

## Moving things

| | |
|---|---|
| **Drag** | moves the selection. Grabbing an unselected element selects it first, so anything moves in one gesture. |
| **Arrow keys** | nudge by 1 design unit (canvas hovered). |
| **Shift + arrows** | nudge by one step of your spacing scale. |
| **Handles** | resize the single selected element from any of eight grips; the opposite edge stays put. |

A drag writes back through whatever placement the element already has: a `Free`
element's position, a `Pin` element's offset, a `Stretch` element's leading
margins. The tab never converts one placement into another behind your back —
the mode you chose is the mode you keep.

**Elements inside a Stack can't be freely dragged**, because the stack places
them. Dragging one **re-orders** it instead, with an insertion caret showing
where it will land.

### Align and distribute

Six align buttons and two distribute buttons.

- With **two or more** selected, they align to the selection's own bounds.
- With **one** selected, it aligns to its parent — which is the whole of
  "centre this panel on the screen", and the reason nobody needs
  `x = (1280 - total) * 0.5` in a script.

Distribute needs three or more; it equalises the gaps and holds the two extremes
still.

---

## Depth

Which of two overlapping panels is in front used to be a property of the order
you happened to create the nodes in: invisible, unauthorable, and unchangeable
without deleting and re-adding a node.

It's now an ordinary integer property — **depth** in the Inspector, `order` in
the `.ron` and in Lua. Lower draws first (further back). Ties keep scene order,
so a layer that never touches it behaves exactly as it always did.

Inside a Stack the same number orders the flow, which is why one drag can do
both jobs.

The **outline panel** lists the layer front-most first. Drag a row to change
depth; right-click on the canvas for *Bring to front* / *Send to back*.

- **👁** toggles `visible` — a real scene property that ships.
- **🔒** locks an element out of canvas picking — an editor-only convenience,
  never saved. Useful for a full-screen background you keep grabbing by mistake.

---

## Designing states

The `hover` / `press` / `focus` / `sel` / `off` buttons force that style state on
the selected element so you can *design* it, instead of discovering at runtime
that your hover colour is invisible.

This runs on the canvas's own copy of the layer, so it can't leak into the saved
scene, and it can't disturb a real hover in the Game view. Transitions still
run, so you also see how the state *arrives*.

---

## Text

Double-click a text element to edit it in place. Escape or click away commits.

---

## Styles from the canvas

Right-click a selection:

- **Copy style / Paste style.** If the source has a named style, pasting assigns
  that name. If it doesn't, pasting copies the *look* — fill, gradient, radii,
  borders, shadow, glow, grain, text treatment, opacity and tint — and leaves
  placement, size and children alone. So it works before you have a style sheet,
  and gets better once you do.
- **Make this a style…** lifts the element's look into a named style in a
  `.uistyle.ron` of your choosing. The file is edited textually, so your
  comments and grouping survive; if you have no sheet yet, one is created at
  `assets/ui/styles.uistyle.ron`.

The element keeps its own values afterwards. A style whose effect you can't see
is a trap.

---

## What this tab deliberately doesn't do

- **It doesn't replace the Scene viewport for world-space canvases.** A panel
  living in the 3D world belongs in the 3D view, and the Scene overlay still
  handles it.
- **It doesn't invent layout.** There's no auto-layout inference, no "fix my
  spacing" button, no snapping to a grid the engine chose. Free placement stays
  the default, and every helper is opt-in.
- **It doesn't ship a look.** No starter theme is applied to anything you make
  here. What your game looks like is your call; this is the surface that makes
  the call cheap to act on.
