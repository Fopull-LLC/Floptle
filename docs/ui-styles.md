# UI styles and tokens

How to stop typing colours onto elements one at a time.

This covers the style system (`assets/ui/*.uistyle.ron`) and the token system
(`assets/ui/*.tokens.ron`). For the element vocabulary itself — shapes,
gradients, 9-slice, text effects — see the Inspector's UI Element section; for
the design rationale see [ui-system-2-proposal.md](ui-system-2-proposal.md).

**The engine ships no styles, no tokens, and no theme.** Everything here is
opt-in and lives in your project. A project with no `.uistyle.ron` behaves
exactly as it did before.

---

## The problem

Without styles, every colour in your project is four floats typed onto one
element. A menu with forty elements has forty of them, and changing the accent
colour is forty edits. Worse: hover and press states have to be written in Lua,
per button, and they drift apart.

```lua
-- the old way, once per button, and no two ever quite matched
function hoverStart(node)
    setFill(math.min(1.0, idle[1] * 1.5 + 0.08), ...)
end
```

With a style, that button is:

```ron
"button": (
    base:    ( fill: "panel", text_color: "ink" ),
    hover:   ( fill: "accent", scale: (1.03, 1.03) ),
    pressed: ( scale: (0.97, 0.97) ),
    transition: ( duration: 0.09, ease: OutCubic ),
)
```

…and zero lines of Lua.

---

## Tokens

Put a `*.tokens.ron` anywhere in your project. Every one found is merged.

```ron
(
    colors: {
        "bg":     (0.02, 0.03, 0.05, 1.0),
        "panel":  (0.07, 0.08, 0.13, 1.0),
        "accent": (1.00, 0.85, 0.35, 1.0),
        "ink":    (0.86, 0.89, 0.95, 1.0),
        "danger": (1.00, 0.35, 0.30, 1.0),
    },
    spacing: { "xs": 4.0, "sm": 8.0, "md": 12.0, "lg": 20.0, "xl": 32.0 },
    radii:   { "sm": 4.0, "md": 10.0, "lg": 18.0, "pill": 999.0 },
    text:    { "caption": 14.0, "body": 18.0, "title": 28.0, "display": 56.0 },
    fonts:   { "ui": "fonts/Inter.ttf", "display": "fonts/Monument.otf" },
)
```

Anywhere a style takes a colour or a number you can write the token name
instead of the value:

```ron
fill: "accent"          // the token
fill: (1.0, 0.85, 0.35, 1.0)   // or the literal — both are valid
```

**A misspelled colour token renders magenta.** That is deliberate: a typo has
to be visible on screen. (A misspelled number token resolves to 0, which shows
up as a collapsed layout.)

### Why bother

The indirection is the smaller half of it. The real value is that defining a
token file forces your project to *have* a spacing scale and a type scale —
and having those is the single biggest structural difference between UI that
looks designed and UI that looks generated. Machine-assembled screens are
recognisable mostly because everything sits at the same visual weight: one
radius, 8 px between everything, one type size. A scale gives you something to
deliberately deviate from.

Five spacing steps and four type steps is plenty. Resist adding more.

---

## Styles

Put a `*.uistyle.ron` anywhere in your project. Each file is a map of names to
styles; all files merge into one namespace, and a duplicated name is reported
in the Console rather than silently shadowed.

```ron
{
    "panel": (
        base: (
            fill: "panel",
            radius: "md",
            border: (0.0, 0.0, 0.0, 2.0),   // bottom rule only
            border_color: "accent",
            grain: ( amount: 0.03, scale: 2.0 ),
        ),
    ),

    "button/primary": (
        base: (
            fill: "accent",
            text_color: "bg",
            radius: "md",
            case: Upper,
            tracking: 1.5,
            pad: "md",
        ),
        hover:    ( scale: (1.03, 1.03), glow: ( color: "accent", radius: 14.0 ) ),
        pressed:  ( scale: (0.97, 0.97) ),
        disabled: ( opacity: 0.35 ),
        focus:    ( border: 2.0, border_color: "ink" ),
        transition: ( duration: 0.09, ease: OutCubic ),
    ),

    "row": (
        base:     ( fill: (0.0, 0.0, 0.0, 0.0), pad: "sm" ),
        hover:    ( fill: (1.0, 1.0, 1.0, 0.06) ),
        selected: ( fill: "accent", text_color: "bg" ),
        transition: ( duration: 0.12, ease: OutCubic ),
    ),
}
```

Pick a style from the **UI Element** section of the Inspector — it's a dropdown
of everything in your sheets, so names are chosen, not typed.

### Driving states and styles from Lua

```lua
local row = find("Row3")

row.style = "button/danger"     -- swap which style paints it
print(row.style)                -- reads back within the same frame

local e = row:getcomponent("UiElement")
e.selected = 1                  -- picks the style's `selected` block
e.disabled = 0
```

`hover`, `pressed` and `focus` are the engine's to set — you never assign
those. `selected` and `disabled` are yours.

---

## The five rules

The whole model, and the reason it doesn't become CSS:

1. **One style per element.** No lists, no classes, no selectors.
2. **Your element's own properties always win.** A property the style doesn't
   mention is left exactly as you authored it in the Inspector. There is no
   specificity to reason about — that's the entire conflict-resolution story.
3. **Inheritance is font, text colour, and the opacity/tint cascade.** Nothing
   else inherits.
4. **The states are a closed set**: `hover`, `pressed`, `disabled`, `focus`,
   `selected`. If you need a sixth, that's a script.
5. **State precedence is fixed**, highest first: `disabled` → `pressed` →
   `hover` → `focus` → `selected` → `base`. A disabled element never lights up
   under the cursor.

`base` always applies; a state block layers on top of it. So `hover` only needs
to say what *changes*.

---

## Transitions

Every style has one:

```ron
transition: ( duration: 0.09, ease: OutCubic )
```

`duration` is seconds; `0` snaps. Interpolation covers the continuous
properties — fills, borders, radii, opacity, scale, rotation, text size and
tracking. Discrete ones (case, font, blend mode, gradient kind) switch
instantly, because half a font is not a thing.

Easings: `linear`, `outCubic`, `inCubic`, `inOutCubic`, `outQuad`, `inQuad`,
`outBack`, `outElastic`. `outBack` overshoots slightly and is what makes a
press feel physical.

Two behaviours worth knowing:

- **Interrupting works properly.** Leaving a button halfway through its hover
  eases back from where it actually is, not from the full hover value. There is
  no pop.
- **First sight snaps.** An element that appears already hovered (or selected)
  settles there instead of animating in, so opening a menu doesn't make every
  row visibly slide into place.

Nothing about transition state is saved. A hover during Play cannot end up in
your `.ron`.

---

## Hot reload

Style and token files are re-read automatically about twice a second. Edit a
token, save, and every screen in the project repaints — during Play too. This
is the loop the system exists for; use it.

---

## What a style can set

| Group | Properties |
|---|---|
| Shape | `fill`, `gradient`, `radius`, `border`, `border_color`, `frame`, `shadow`, `glow`, `grain`, `blend` |
| Element | `opacity`, `tint`, `rotation`, `scale` |
| Text | `text_color`, `text_size`, `tracking`, `line_height`, `case`, `font`, `text_stroke`, `text_shadow` |
| Layout | `pad`, `gap` (on a stack) |

`fill`, `border_color`, `text_color` and `tint` take a colour token or a
literal. `text_size` takes a `text` token or a number; `pad`/`gap` take a
`spacing` token or a number; `radius` takes a number, a `radii` token, or four
numbers `(TL, TR, BR, BL)`.

Layout *placement* is deliberately absent: a style says what things look like,
not where they go. Where they go is the designer's, in the viewport.

---

## Frames — a sprite instead of a border

`border` strokes a rectangle. `frame` puts a **9-sliced sprite** there instead,
which is how a pixel-art project gets an edge it drew rather than one the
renderer computed.

```ron
"panel": (
    base: (
        fill: "ink",
        border_color: "silver",
        frame: (
            texture: "textures/ui/frames.png",
            uv: (0.25, 0.0, 0.5, 0.5),
            slice: (0.25, 0.25, 0.25, 0.25),
        ),
    ),
),
```

- **`uv`** is a `(min_u, min_v, max_u, max_v)` window into the texture, so every
  frame in a game can live in one atlas — one texture, one draw call, and a new
  panel style costs a line rather than an asset.
- **`slice`** is the corner inset as a fraction of that window. The corners keep
  their drawn pixel size and only the middles stretch.

**A frame is tinted by `border_color`.** That is the same channel a drawn border
uses, so one white sprite becomes a bright focused edge and a dim idle one with
no second asset, and the frame picks up the style's hover/focus transition for
free. A style that sets `frame` should set `border_color` in the same block —
the default is opaque black, which is an edge you can see but did not choose.

From Lua the three properties are `frame`, `frameUV` and `frameSlice`.

### ⚠ The size floor

A corner patch **never stretches**. An element smaller than two of them has no
middle left, and the renderer's answer is to abandon the nine patches and draw
the whole sprite as one stretched quad. That does not look like a small frame —
it looks like a smear.

So the floor is **twice your corner size in both axes**. Work it out once from
the atlas: a 48px cell at `slice: 0.25` is a 12px corner, so nothing under ~24
units should carry that frame. Menu rows, meter tracks and pips are usually
under it and want a plain `border` instead. Switching to a different frame cell
does not help — the fallback is about size, not art.

---

## A note on file format

These files are parsed with RON's implicit-`Some`, so you write

```ron
fill: "accent"
```

not `fill: Some("accent")`. Both parse; the first is what you should write.
