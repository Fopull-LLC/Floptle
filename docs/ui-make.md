# Screens from data — `ui.make`

Building UI whose *shape* depends on data: a roster of four fighters or nine,
an inventory of whatever the player is carrying, a lobby list that arrives over
the wire.

For hand-designed screens use the scene and the [◫ UI tab](ui-tab.md) — that is
still the main way to author UI, and this is not a replacement for it. For "keep
this label showing that number" use [`ui.bind`](scripting.md). For "there should
be N of these rows" use the repeater. Reach for `ui.make` when the scene file
can't hold the tree because the tree doesn't exist until the game is running.

---

## The short version

```lua
ui.make(find("Crew Panel"), {
    "col", inset = 0, style = "panel", gap = 10, pad = 16,
    { "text", text = "CREW", style = "caption" },
    { "col", w = "100%", gap = 6, items = crew,
        function(m)
            return {
                "button", key = m.id, style = "row", dir = "row", gap = 10,
                onClicked = function() standDown(m.id) end,
                { "box", w = 26, h = 26, radius = 13, text = m.name:sub(1, 1) },
                { "text", text = m.name },
            }
        end,
    },
})
```

`ui.make(container, tree)`. The container is any element or UI layer node; the
tree is one element table, or an array of them.

Call it **when the data changes**, not every frame. It is safe every frame — it
reconciles rather than rebuilds — but it re-reads your table each time, and
nothing about the screen needs that.

This is a live demo, not a sketch: it is the right-hand panel of
`assets/scenes/ui_demo.ron`, whose entire contents are in `ui_demo.lua`. The
scene holds one empty node saying where the panel sits.

---

## An element

```lua
{ "kind", prop = value, ..., child, child, ... }
```

The first entry is the kind, when it's a string. Everything else positional is
a **child**; everything named is a **property**.

| Kind | Is |
|---|---|
| `box` | a rounded rect, transparent until something paints it |
| `row` / `col` | a box whose children flow across / down |
| `text` | a text run, with no shape behind it |
| `image` | a textured rect |
| `button` | a box that takes clicks, and that a direction press can reach |
| `field` | an editable text field (implicitly focusable) |
| `slider` | a value track whose `part` children fill and ride |
| `scroll` | a clipped, scrollable view of its children |

Every kind is reachable from `box` by setting properties — `col` is a box with
a stack, `button` is a box with `button = true`. The kind is shorthand, not a
separate class of thing, and leaving it out gives you a box.

Sub-specs appear on demand, so `{ "box", text = "hi" }` is a label with a
background and `{ "box", gap = 8 }` is a container. Nothing has to be declared
before it is used.

### Two keys of the builder's own

- `key = "id"` — reconciliation identity (see below).
- `name = "Save Button"` — the node's name in the hierarchy. Names matter:
  masks, scrollbars and nav overrides address elements by name. Without one,
  the name is the key, or the kind and position (`text2`).

---

## Lists — `items`

Give a container `items` and a function child, and the function runs once per
item:

```lua
{ "col", gap = 6, items = inventory,
    function(item, i) return { "text", key = item.id, text = item.name } end }
```

The function receives `(item, i)` with `i` 1-based, and returns an element
table — or **`nil` to skip that item**, which is how a filtered list stays one
expression.

A function child *without* `items` is called once, and is the natural way to
write a conditional part of a screen:

```lua
function() if #offDuty > 0 then return { "button", text = "RECALL" } end end
```

---

## Behaviour — `on…`

Any UI hook, with `on` in front and the first letter capitalised:

```lua
{ "button", text = "LAUNCH", onClicked = function(node) launch() end }
{ "field", placeholder = "CODE", onSubmitted = function(node) join(node.text) end }
```

`onClicked`, `onPressed`, `onReleased`, `onHoverStart`, `onHoverEnd`,
`onChanged`, `onSubmitted`, `onCancelled`, `onFocusEnter`, `onFocusExit`,
`onDragStart`, `onDragMove`, `onDragEnter`, `onDragOver`, `onDragLeave`,
`onDragCancel`, `onDropped`.

The handler gets the element's node handle, exactly as a script's own
`clicked(node)` does — and a gamepad submit fires `onClicked` the same way a
mouse does. Re-describing the screen replaces the handlers, because the closures
are made fresh on each call and capture that call's values.

An element can still carry an ordinary script (drop one on the container's
prefab, or use a repeater for rows that need real behaviour of their own). Both
fire: the script's hook first, then the described handler, then any
[`ui.on`](scripting.md#one-script-for-a-whole-screen--uion--uievents) listeners
— which is how a hand-placed screen gets the same "all in one script" shape a
described one has for free.

---

## Reconciliation — what a second call does

Calling `ui.make` again **spawns and destroys only the difference**. Rows that
stay keep their entity, and with it every scrap of runtime state hanging off it:
hover, focus, scroll position, in-flight style transitions, what was typed into
them. A builder that rebuilt the subtree would give you a screen that flickers
and forgets — which is exactly the hand-rolled behaviour this replaces.

Described elements match existing ones **by key** where there is one, and **by
position** where there isn't. A kind change never matches: a `text` becoming an
`image` is a different element.

Use keys whenever a list can be re-sorted or filtered:

```lua
-- without a key, "Ana, Bo, Cy" → "Cy, Ana, Bo" moves the labels but leaves
-- every row's hover and selection one slot out of place
{ "row", key = member.id, ... }
```

The described order is the draw and flow order, so a reordered list reorders on
screen without touching `order` yourself.

### What resets and what doesn't

**The description is authoritative.** A property your table stops mentioning
goes back to the element's default — otherwise deleting a line from your table
would leave its effect on screen forever, and the table would stop describing
what you see.

Four things are kept anyway, because none of them is something the description
*said*:

- a scroll view's position
- what the player typed into a field
- which toggle or radio chip is selected
- a **draggable** slider's value (a display-only meter is driven by the game, so
  it follows the description)

When the description does speak — `text = "reset"`, `selected = true` — the
description wins.

### Elements you placed by hand

Reconciliation only ever considers children the builder itself made. An element
you put in the scene under the same container is never matched, never patched
and never destroyed, so a data-driven list can live inside a designed panel.

---

## Properties

Names are the same ones a script writes through
`node:getcomponent("UiElement")`, plus the structural ones a live field write
can't express. **A name that isn't in this list raises**, with a suggestion —
a declarative screen that silently ignores a line is worse than one that stops.

**So does a value.** Where a property takes a fixed set of words, a word outside
it raises too, naming what it got and what it takes. It used to answer with a
default: `pin = "topCenter"` meant `topLeft`, silently, and four HUD elements
would pile into one corner while looking for all the world like a layout bug.
The sets are listed below.

**Placement** — `pin`, `inset` (fill the parent with a margin), `stretch`
`{minX, minY, maxX, maxY}`, `margin`, `x`, `y`, `pos`, `order`.

`pin` takes one of nine anchors:

| | | |
| --- | --- | --- |
| `"topLeft"` | `"top"` | `"topRight"` |
| `"left"` | `"center"` | `"right"` |
| `"bottomLeft"` | `"bottom"` | `"bottomRight"` |

The four middle edges also answer to the longer spelling people reach for —
`"topCenter"`, `"bottomCenter"`, `"leftCenter"`, `"rightCenter"` — and `centre`
works anywhere `center` does.

**Size** — `w`, `h`, `size`, `minW`, `minH`, `maxW`, `maxH`. A number is design
units; `"50%"` is a fraction of the parent; `"grow"` / `"grow 2"` shares the
leftover space in a stack; `"fit"` wraps the content.

**Stack** — `dir` (`"row"` / `"column"`), `gap`, `pad`, `justify` (`"start"`,
`"center"`, `"end"`, `"between"`), `align` (`"start"`, `"center"`, `"end"`,
`"stretch"`). Any one of them makes the element a container.

**Shape** — `fill`, `radius`, `border`, `borderColor`, plus the indexed forms
(`fillR`, `radiusTL`, `borderB`, …). A quad takes a scalar for all four or a
list: `radius = {8, 8, 0, 0}`.

**Text** — `text`, `textSize`, `textColor`, `textAlign`, `textValign`,
`tracking`, `lineHeight`, `font`, `wrap`, `maxLines`, `case`, `overflow`,
`textFit`.

**Image** — `texture`, `tint`, `cols`, `rows`, `cell`, `slice`, `tiling`,
`imageFit` (`"stretch"`, `"contain"`, `"cover"`).

**Interaction** — `button`, `toggle`, `group`, `selected`, `disabled`,
`focusable`, `draggable`, `dropTarget`, `tooltip`, `tooltipBox`, `part`
(`"fill"` / `"handle"`), `navUp` / `navDown` / `navLeft` / `navRight`.

**Look** — `style`, `visible`, `opacity`, `groupTint`, `rotation`, `scale`,
`pivot`, `shader`.

**Field** — `placeholder`, `maxLen`, `numeric`, `upper`, `mask`.

**Slider** — `min`, `max`, `value`, `interact`, `flip`, `sliderDir`.

**Scroll** — `scrollX`, `scrollY`, `scrollSpeed`, `scrollDrag`, `scrollbarFor`,
`scrollbarAxis`.

**Repeater** — `template`, `count`. A made container can still repeat a prefab:
`ui.make` describes the shape of the screen, the repeater fills a list with rows
that carry their own art and scripts.

Colours take a `color(...)`, a `"#rrggbb"` string, a plain number (a grey), or a
`{r, g, b}` list. The key order of your table never matters.

---

## Rules and edges

- **Play only**, same as the repeater. Made elements are runtime content, and an
  editor action that conjured them into the open scene would put engine-built
  nodes in a file you are about to save. In edit mode the call is dropped with a
  Console warning.
- **A made box is invisible until something paints it.** Not white — a builder
  that painted slabs until told otherwise would be choosing a look.
- **A `button` kind is focusable** unless you say otherwise. That is a behaviour
  default, not a look: what focus *looks* like is still your style's `focus`
  block, and the engine draws no ring.
- **The ◫ UI tab shows an empty container** for a made subtree, because it isn't
  built until Play. Design the frame — where the panel sits, how big it is — in
  the scene, and let the description fill it. That is the intended split.
- Made nodes are never serialised, and Stop removes them with the rest of the
  play session.

---

## Also

- [ui-styles.md](ui-styles.md) — tokens, styles, states, transitions
- [ui-tab.md](ui-tab.md) — the authoring canvas
- [ui-navigation.md](ui-navigation.md) — focus, fields, drag & drop, tooltips
- [ui-demo.md](ui-demo.md) — the demo scene, including the crew panel above
- [scripting.md](scripting.md) — `ui.bind`, colours, the repeater
