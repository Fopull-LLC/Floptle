# Keyboard and gamepad navigation

Making a menu that isn't mouse-only.

This covers focus and directional navigation. For the element vocabulary see
the Inspector; for styles and states see [ui-styles.md](ui-styles.md); for the
authoring canvas see [ui-tab.md](ui-tab.md).

---

## The short version

Tick **focusable** on the elements a player should be able to reach. That's it —
directions move between them, submit fires their `clicked` hook, and a project
with no input configuration at all responds to arrows, the d-pad, the left
stick, Enter and Escape.

```ron
(
    // a menu button
    place: Free(pos: (200, 300)),
    size: (Fixed(280), Fixed(64)),
    button: true,
    focusable: true,
    style: "button/primary",
)
```

The style's `focus` block is what makes it visible:

```ron
"button/primary": (
    base:  ( fill: "panel", text_color: "ink" ),
    focus: ( border: 3.0, border_color: "accent", scale: (1.04, 1.04) ),
    transition: ( duration: 0.08, ease: OutCubic ),
)
```

---

## Why there is no built-in focus ring

**Because a ring is a look, and the engine doesn't ship looks.**

A hard-coded rectangle would be the wrong shape on a round button, the wrong
colour on half of all games, and impossible to remove on the rest. The `focus`
state resolves through the ordinary style system, so focus can be a border, a
glow, a fill change, a scale pop, a shifted gradient, an arrow that slides in —
whatever your game is.

The trade is that a focusable element with no `focus` block shows nothing. That
is deliberate and it is the correct default; the ◫ UI tab's **⇹⇳ nav overlay**
is how you check reachability while building, and it costs nothing at runtime.

---

## How a direction resolves

From the focused element, in the pressed direction:

1. A **`nav` override** naming an element, if you set one.
2. Otherwise, the **nearest focusable ahead** in that direction, from the solved
   rects — with "straight ahead" beating "closer but off to the side", so a
   column of buttons walks down itself instead of wandering into whatever is
   diagonally nearby.
3. Otherwise nothing — unless the layer has `nav_wrap`, in which case it comes
   back on the far side.

It's geometry, so it keeps working when you move a button. You don't maintain a
list.

### When geometry is wrong

Some cases geometry genuinely can't know: a grid that should wrap at the end of
each row, a Back button reachable from anywhere on the screen, two columns that
must not be treated as one field. Name the target:

```ron
focusable: true,
nav: ( right: "Slot 1", down: "Back" ),
```

Each direction is independent, so you override only the edges that need it and
leave the rest to the geometry. A name that doesn't resolve to a focusable
element is ignored rather than swallowing the press.

---

## Bindings

The engine looks for these actions in your `input.ron`:

| Action | |
|---|---|
| `UiUp` `UiDown` `UiLeft` `UiRight` | discrete directions |
| `UiMove` | a 2D axis (stick), as an alternative |
| `UiSubmit` | fires the focused element's `clicked` hook |
| `UiCancel` | fires its `cancelled` hook |

**If you define none of them**, the engine falls back to arrows / d-pad / left
stick / Enter+Space / Escape, so a new project's menu works before anyone opens
the Input settings. **If you define any of them**, your map takes over
completely — a half-overridden control scheme is worse than either.

Nothing is written to `input.ron` on your behalf.

### Auto-repeat

Holding a direction moves once, waits, then rolls. Both numbers live on the
**layer**, because a fast action menu and a long settings list genuinely want
different ones:

```ron
UiLayer(
    design_height: 720.0,
    nav_delay: 0.35,     // seconds before it starts repeating
    nav_repeat: 0.12,    // seconds between repeats
    nav_wrap: false,     // running off the end comes back on the other side
)
```

Changing direction restarts the delay, so a held press doesn't machine-gun the
moment you change your mind.

---

## Submit is a click

`UiSubmit` on the focused element fires the **same `clicked` hook a mouse
fires**, preceded by `pressed` and `released`. A button written for a pointer
works with a pad with no second code path:

```lua
function clicked(node)
    scene.load("arena")
end
```

Clicking with the mouse also *focuses* the element, so a player who reaches for
the mouse mid-menu and goes back to the pad carries on from where they clicked.

---

## Scripting

```lua
-- read
if node.focused then ... end
local current = ui.focused()          -- a node, or nil

-- write
ui.focus(find("Play"))                -- move the ring
ui.focus(nil)                         -- nothing focused
```

Hooks on the element's scripts:

| | |
|---|---|
| `focusEnter(node)` | the ring arrived |
| `focusExit(node)` | the ring left |
| `clicked(node)` | submit, or a mouse click |
| `cancelled(node)` | `UiCancel` while focused |

Focus is engine state, not a component. It is never saved into a scene, it is
cleared when Play stops, and there is exactly one way to move it — which is why
there is exactly one place to look when it goes somewhere surprising.

---

## Rules worth knowing

- **One element is focused at a time**, across every layer. The **front-most
  layer that has anything focusable** owns it, which is what makes a modal over
  a menu behave.
- **A hidden or disabled element is not focusable**, and neither is anything
  inside a disabled one — being able to press a button that visibly can't be
  pressed is a bug, not a feature.
- **A focus that stops resolving** (the screen changed, the element was hidden)
  falls back to the first focusable rather than vanishing.
- **The first direction press focuses the first element** rather than moving
  from nowhere, so a screen can open with nothing focused and still respond.

---

## Checking it without launching the game

In the ◫ UI tab, turn on **⇹⇳**. Every focusable element gets a dot, and the
selected one gets an arrow to wherever each direction leads — computed with the
same code the game runs, including your `nav` overrides (drawn thicker, because
they're deliberate) and the layer's wrap setting.

If a layer has nothing focusable, the overlay says so. That is the single most
common cause of "my menu ignores the controller".

---

## Toggles, radio groups, and scrolling

Three things every menu needs that used to be a script each.

### Toggle

`toggle: true` — clicking flips `selected`. A checkbox, a mute button, a
filter chip. What "on" looks like is your style's `selected` block; the engine
draws no tick and no switch.

### Radio groups

`group: "difficulty"` — clicking selects this element and deselects everything
else in the same layer with that group name. Tabs, difficulty pickers, weapon
slots, a character-select grid.

Groups are scoped to a **layer**, so two screens can reuse a name without
interfering. A group of one is a toggle that can't be turned off, which is
occasionally exactly what you want.

Both work identically from a mouse click and a gamepad submit, because both
paths fire the same `clicked`.

### Scrolling

A scroll view scrolls on **both axes**. The wheel drives whichever axis has
travel — so a horizontal strip of cards scrolls with an ordinary wheel and
nobody has to know why — and shift forces sideways.

```ron
scroll: ( speed: 48.0, drag: true ),
```

`drag` pans the content by dragging its background. Off by default: in a view
full of buttons, a drag that scrolled would fight every press.

Scripts read and write `UiElement.scrollX` / `scrollY`.

### Scrollbars are your elements

There is no built-in scrollbar, for the same reason there is no built-in focus
ring: a scrollbar is one of the most style-defining things on a screen.

Instead, any element can *become* a track:

```ron
// the track
( scrollbar: ( target: "Inventory", axis: Column ), shape: (...) )
//   └── child, part: Handle → the thumb
```

The thumb's length becomes the **visible fraction of the content**, so the bar
reads as "how much of this list am I seeing", not just "where am I". Its
cross-axis geometry is left exactly as you authored it — a 4-unit hairline and
a chunky 20-unit slab are both yours to make. Grabbing anywhere on the track
jumps there and keeps tracking.

It reuses the slider's `part: Handle` machinery, so it's the same idea you
already know from progress bars.
