# Floptle UI — a designer-first game UI system (proposal)

**Status: Proposed** — for Ty's review. Decisions to resolve are collected in §13.

The next big system after netcode. Goal in one sentence: **making a game menu or
HUD should feel like arranging things, not programming a layout engine** — and
the moment logic is needed, it should be a few lines of the same Lua the rest of
the engine speaks.

---

## 1. Why UI in other engines is miserable (the diagnosis)

Every design choice below traces back to one of these. If a feature doesn't kill
one of these pains, it doesn't belong in v1.

| # | Pain | Where you've felt it |
|---|------|----------------------|
| P1 | **Manual positioning math.** Anchors/pivots/`UDim2{0.5,-100,...}` — you compute pixel offsets in your head, and it breaks on the next aspect ratio. | Unity RectTransform, Roblox UDim2 |
| P2 | **Design and logic live in different worlds.** Build visuals in one tool/file, then wire events and find-element calls in another; renaming a button breaks scripts silently. | Unity (prefab + C# + serialized events), UI Toolkit (UXML + USS + C#) |
| P3 | **No real styling.** Colors/fonts/spacing copy-pasted per element; a theme change is an afternoon of clicking. Or the opposite failure: full CSS with specificity wars. | Roblox (nothing), UI Toolkit (CSS-ish) |
| P4 | **Slow iteration.** Change → recompile/replay → navigate back to the screen → look. Nobody polishes UI under that loop. | Unity especially |
| P5 | **State→UI plumbing.** `healthBar.value = hp` sprayed through update functions, or event-listener spaghetti; UI drifts out of sync with game state. | everywhere |
| P6 | **Juice is expensive.** A hover grow, a press flash, a fade-in — each becomes a coroutine/tween library dependency and ten lines per element. | Roblox TweenService boilerplate, Unity animators for buttons |

## 2. Design pillars

1. **It's just the scene.** UI elements are scene nodes — same hierarchy panel,
   same Inspector, same undo, same copy/paste, same `.ron` files, same script
   attachment, same modular-component model. No parallel "UI document" world
   (kills P2). A menu is a sub-scene; spawning one is the machinery
   `player.ron` already uses.
2. **The designer places things; auto-layout is a tool, not a regime.**
   Free placement is the default — drop an image or shape anywhere on the
   layer and it stays where you put it, sized how you sized it. When you
   *want* flow (a settings list, an inventory grid), you opt a container into
   auto-layout and it stacks its children for you. Pinning to screen corners
   is a preset, not arithmetic (kills P1 without taking control away).
3. **Direct manipulation.** You edit UI *in the Game view*, at game resolution,
   live — drag to reorder inside a stack, drag handles to resize, double-click
   a label to retype it. During Play too (kills P4).
4. **No imposed look — and still no CSS.** The engine ships zero UI art: your
   UI is *your* textures from `assets/textures/`, generic shapes, transparency,
   and text. Styling is plain properties on the element; optionally a project
   can define named tokens (its own colors/spacing) to keep itself consistent.
   Element states (hover/pressed/disabled) and their transition times are
   *data on the style*, not code. No selector language, no cascade debugging
   (kills P3, P6 — without deciding what your game looks like).
5. **Reactive by default.** A property can be *bound* to a Lua expression —
   `hp / maxHp` — and the engine keeps it current. HUDs read like a sentence,
   and `synced` vars make them multiplayer-correct for free (kills P5).
6. **Ship-ready from day one.** Retained tree, dirty-flag layout, one batched
   draw pass. Runs in exported builds; eventually *replaces* the built-in F1
   multiplayer menu with a game-made one (dogfooding).

## 3. The model

A screen of UI:

```
Scene
└─ HUD                      ← Matter::Gui (a UI layer: screen-space canvas)
   ├─ TopBar                ← Element: stack row, pinned top, pad 12, gap 8
   │  ├─ HpBar              ← Element: two rects (track + fill), YOUR widget
   │  └─ Coins              ← Element + Text
   └─ PauseMenu             ← Element: stack column, centered, hidden
      ├─ Title              ← Element + Text
      ├─ ResumeBtn          ← Element + Text + Interact  ("a button" = those three)
      └─ QuitBtn            ← Element + Text + Interact
```

- **`Matter::Gui`** — the layer root. Owns screen-space projection, scale mode,
  and the native/retro resolution choice (§6). Multiple layers compose (HUD +
  pause menu + damage numbers), each with a z-order.
- **`Matter::Element`** — the one UI node kind. What an element *is* comes from
  additive components, exactly like the modular Inspector today:
  - **Layout** — position/size, and (only if you opt in) how it arranges
    children (§4). Every element has one; the default is "where you put it,
    the size you gave it."
  - **Shape** — the visual primitive: rectangle (corner radius 0 → sharp,
    high → circle/pill), fill color, border, opacity/transparency, shadow,
    per-state overrides + transitions (§5). Optional (an invisible group
    needs none).
  - **Text** — content, font, size, color, wrap, align.
  - **Image** — any texture from `assets/textures/` (the existing registry),
    9-slice insets for stretchable panels, tint, opacity.
  - **Interact** — makes it clickable/hoverable/draggable/focusable; fires
    script hooks.
  - **Scripts** — the same additive script list every node has.

  Shapes + images + text + transparency IS the widget vocabulary — **the
  engine ships no premade UI assets**. A slider is three elements you can
  build in a minute: a track rect, a fill rect, a knob rect with a drag
  Interact and five lines of Lua. The docs walk through exactly that instead
  of handing you one, because a shipped slider would look like *our* game,
  not yours.
- **Widgets are sub-scenes — yours.** Once you've built that slider, save it
  as a small `.ron` of elements + a script with `defaults` (the existing
  tunables machinery = its properties in the Inspector) and reuse it
  everywhere; your design system is a folder in your project. No prefab
  system to build — scene spawning already exists, and nothing about your
  widgets is engine-blessed.

## 4. Layout (the P1 killer — with the designer in charge)

Three modes on the Layout component, friendly vocabulary over a real flexbox
engine (the `taffy` crate — proven in production Rust UI):

- **Free** *(the default)* — the element sits where you dragged it, at the
  size you gave it, relative to its parent. Full control, art-directed
  screens, no engine opinion. Positions/sizes can be px or % of parent, so a
  freely-placed screen still survives resolution changes.
- **Pin** — Free plus an edge: stick to a 9-point preset (top-left … center …
  bottom-right) + an offset, so HUD corners follow the window. Two numbers,
  not four anchors and a pivot.
- **Stack** *(opt-in per container)* — when you *want* flow: `direction`
  (row/column), `gap`, `pad`, `align`, `justify`, `wrap`. Children arrange
  themselves; reordering is dragging in the hierarchy *or in the viewport*.
  This is a convenience you reach for on lists, grids, and button columns —
  never something the system forces on you.

Sizing per element: `fixed(px)`, `pct(%)`, `fit` (content), and inside stacks
`grow(weight)` (share leftover space). That's the whole vocabulary —
deliberately ~8 concepts, not CSS's 40. Free placement gives control; Stack
exists so the *tedious* screens (settings lists, inventories) don't cost it.

## 5. Style, states, and (optional) tokens (the P3/P6 killer)

- **Plain properties first.** Fill, border, radius, opacity, texture, tint —
  set directly on the element, no indirection required. The engine imposes no
  look whatsoever; "unstyled" UI is literally just your shapes and textures.
- **States on the style, not in code**: `hover`, `pressed`, `disabled`,
  `focus` blocks override base properties; a `transition` duration+easing per
  block animates the change. A juicy button — grow 4% on hover in 80 ms, dip
  on press — is **zero lines of Lua**. This works on raw shapes and on your
  own widgets alike.
- **Tokens are opt-in, project-defined.** A project *may* create a token
  asset (`assets/ui/*.tokens.ron`) naming its own colors/spacing/fonts, and
  any style property can reference one — so a palette change hits every
  screen. No engine-shipped theme, no default palette, nothing to fight.
- **No selectors, no cascade.** Inheritance is exactly: children inherit font
  and text color unless overridden. Everything else is explicit per element
  or comes from your widget's defaults. Predictability beats power here.

## 6. Resolution, scaling, and the retro look

- Logical UI units with a **design height** (project setting, e.g. 720): the
  layer scales uniformly to the window, so text/spacing hold their proportions
  at any resolution. `pct` sizes handle aspect differences; Pin handles edges.
- Per-Gui-layer **`resolution: native | retro`**:
  - `native` (default) — crisp text/panels composited after the post chain.
  - `retro` — the layer renders *into the retro target before the upscale*:
    chunky pixels that match the world, per-retro-pixel AO-era aesthetic.
    Bitmap fonts (§7) shine here.
- Safe-area inset support from day one (cheap now, painful retrofitted).

## 7. Text

- TTF via the modern wgpu text stack (`cosmic-text` shaping + `glyphon`
  atlas/rendering) — correct kerning/wrapping, cheap glyph caching.
- **Bitmap grid fonts as first-class assets** (a PNG + cell size): the retro
  option, pixel-perfect in `retro` layers — and trivially your own (draw a
  glyph sheet in any pixel editor, drop it in `assets/`).
- Text needs *some* font before you've added one, so the engine embeds a
  single neutral fallback — a technical necessity like the checker texture on
  an untextured cube, not a look. Real projects drop their own TTF/bitmap
  fonts into `assets/` and never see it.
- Text *input* (TextField) is deliberately **phase 5** — carets, IME,
  selection are a project of their own; menus and HUDs don't need them.

## 8. Lua API (the P2/P5 killer)

Same house style as everything else: camelCase, node-centric, tiny surface.
Elements are nodes — `find`, `node.parent`, `:children()`, `:getscript()`
already work. New global: `ui`.

**Events — script hooks on the element** (designer wires the script in the
Inspector; consistent with `start`/`update`/`pressed` conventions):

```lua
-- resume_button.lua, attached to ResumeBtn
function clicked()
    find("PauseMenu").visible = false
end

function hoverStart() ui.sound("tick") end   -- optional juice beyond the style
```

**Or programmatic, from any script:**

```lua
find("QuitBtn").onClick(function() net.leave() quitToTitle() end)
```

**Bindings — state to UI, one line per fact** (evaluated per frame, diffed, so
they're honest and cheap; `synced` vars make them replicate):

```lua
-- hud.lua, attached to the HUD layer
defaults = { hpBar = ui.ref(), coins = ui.ref() }  -- node slots: drag the
                                                   -- elements in via the Inspector
function start()
    ui.bind(params.hpBar, "value", function() return synced.hp / synced.maxHp end)
    ui.bind(params.coins, "text",  function() return ("%d ¢"):format(synced.coins) end)
end
```

(References resolve zero times per frame: `params.hpBar` is a live handle
wired in the Inspector, and a binding captures its element once — see §8.1.
`find` still exists for quick scripts; it just stops being the taught path.)

**Dynamic construction — declarative, for lists and spawned things:**

```lua
for i, item in ipairs(inventory) do
    ui.make(find("ItemList"), {
        "row", gap = 8, pad = 4,
        { "image", texture = item.icon, size = 32 },
        { "text",  text = item.name, grow = 1 },
        { "text",  text = item.count },
        onClick = function() equip(i) end,
    })
end
```

**Element properties** (read/write like any node): `visible`, `text`,
`value`, `texture`, `opacity`, plus `element:setStyle("pressed")` overrides for
special moments. Position/size are scriptable too (Free elements are yours to
animate) — but layout-managed children can't be teleported out of their stack.

### 8.1 Getting references without paying for `find` (Ty's concern — he's right)

Today `find("Name")` is a **linear scan with string compares** over the scene
order, and `node:find` walks the subtree. At current scene sizes one call is
microseconds — but a UI-heavy game (hundreds of elements, dozens of scripts,
some calling it per frame) turns that into real waste, and strings are fragile
besides. Three layers, in order of importance:

1. **Node-reference script params (the designer-first fix).** Script
   `defaults` gain a node-ref type: `defaults = { hpBar = ui.ref() }` shows a
   node slot in the Inspector — drag the element in (the BoneAttach picker
   pattern already exists). The script reads `params.hpBar` as a live node
   handle: **zero strings, zero lookups, survives renames**, and the wiring
   is visible in the Inspector instead of buried in code. This becomes the
   *taught* way to reference anything — UI or not.
2. **A name index in the engine (makes even "bad" code fast).** The scene
   mirror keeps `name → entities` in a hash map, maintained on
   spawn/despawn/rename, so `find` becomes O(1) regardless of scene size.
   Ships with UI phase 1 but benefits every existing script immediately.
3. **The cache idiom (docs + examples).** Every example resolves references
   once in `start()` — never in `update`. `ui.bind` already follows this
   shape: the closure captures the element handle at bind time; only your
   expression runs per frame.

Net effect: the fast path (refs) has no lookup at all, the lazy path (`find`)
stops scaling with scene size, and per-frame `find` in hot loops becomes a
documented anti-pattern the IDE can nudge about later.

## 9. The editor experience

- **Edit in the Game view.** Selecting a Gui layer switches the viewport into
  UI-edit: elements outline on hover, click selects (Inspector shows the
  modular components), drag moves within Free/Pin or *reorders* within a
  Stack (insertion caret, like dragging a browser tab), handles resize,
  double-click edits text inline. Esc/click-out returns to the 3D scene.
- **Add menu ⏵ UI** — layer, shape, image, text, empty stack. That's the whole
  menu: primitives, not widgets. Dropping one of *your* widget `.ron`s from
  Assets into the hierarchy instantiates it (exact same gesture as
  models/scripts today).
- **Live during Play** — styles, layout, text all hot-apply (the play-snapshot
  restore on Stop applies as usual: play-time experiments revert unless saved).
- **Token editor** — if the project defines tokens, a small panel lists them
  with pickers; edits hit the whole screen live. (The Material Editor
  pattern, reused.) Absent tokens, there's nothing to configure.
- **"Built from primitives" example scene** — a `--play`-able scene (docs
  material, not engine assets) constructing a button, slider, and health bar
  from raw shapes; doubles as our test bed and the §3 tutorial.

## 10. Rendering & performance

- New `floptle-ui` crate: tree + layout + style resolution, renderer-agnostic
  output = one **draw list** (quads, glyphs, clips).
- One instanced pipeline: rounded-corner SDF quads (fill/border/shadow in
  shader), 9-slice images from the existing texture registry, glyph atlas
  quads; scissor stacks for scroll clipping. Hundreds of elements ≈ one draw
  call per layer.
- **Dirty-flag discipline**: layout recomputes only for subtrees whose
  size-affecting props changed; style state changes touch only instance data;
  bindings diff before writing. Idle UI costs ~zero CPU and re-uploads nothing.
- Draw-list consumers: the Game view (editor), player mode, and — later — the
  shader system can inject custom UI materials (the WGSL seams stay clean per
  ADR-0007 thinking).

## 11. Integration map

- **Netcode**: bindings read `synced` naturally; `net.on("playerJoined", …)`
  can `ui.make` a scoreboard row. The parry game's HUD is the acceptance test.
- **Animation system**: phase 6 exposes element properties (opacity, offset,
  scale) as animatable channels on the existing timeline — cutscene-grade UI
  motion without new tooling.
- **Particles**: a `screenSpace` emitter flag someday for menu sparkle; not v1.
- **Exports**: player mode gains "the game draws its own menus"; once a
  project ships a main menu, the built-in F1 panel demotes to a debug overlay.
- **IDE**: `ui.*`, hooks (`clicked`, `hoverStart`…), and element properties go
  into all three doc surfaces (EmmyLua stubs, completion, scripting.md §16)
  the same day the API lands — house rule.

## 12. Phased roadmap

| Phase | Lands | Playable proof |
|-------|-------|----------------|
| 1 | `floptle-ui` crate: Gui/Element, Stack/Pin/Free layout (taffy), Style (no states), Text (TTF), Image, draw-list renderer; hierarchy/Inspector/RON integration; Add ⏵ UI | a static title screen + HUD laid out entirely in-editor |
| 2 | Viewport UI-edit mode (select/drag/reorder/resize/inline text) | build phase 1's screen with zero Inspector number-typing |
| 3 | State blocks + transitions; optional project tokens + token editor; node-ref script params + the find() name index | juicy hoverable menu built from raw shapes, zero code |
| 4 | Lua: hooks, `onClick`, `ui.bind`, `ui.make`, visibility; IDE surfaces | pause menu + live HP bar over the parry dummy fight |
| 5 | Widgets-as-sub-scenes (yours) + scroll containers + drag Interacts + bitmap fonts + `retro` layers; safe areas; the primitives-only example scene (slider walkthrough) | the multiplayer join screen, made *in the engine* from raw shapes, replacing F1 for a real host/join |
| 6 | Animation-timeline channels; TextField; polish (focus nav, gamepad) | a settings screen navigable by controller |

Phases 1–3 are the "designer-first" bet paying off *before* scripting even
enters; that ordering is on purpose.

**Status (2026-07-08):** phase 1 shipped, plus early pulls from later phases by
request: text vertical anchoring + fit-to-rect dynamic sizing; **Slider**
(Add ⏵ UI ⏵ Slider — a track whose Fill/Handle children are ordinary elements
you retexture and arrange; `min`/`max`/`value`/axis/flip); **UI masks** (an
element clips chosen target nodes + subtrees to its rounded rect; earliest mask
in scene order wins a contested target, the Inspector warns on the loser); and
the first slice of the Lua API — `node.text` and
`getcomponent("UiElement" / "UiSlider" / "UiLayer")` (scripting.md §7).

## 13. Decisions

**Resolved by Ty (2026-07-08):** the designer keeps full control — free
placement is the default and auto-layout is opt-in per container (§4); the
engine ships **no premade UI assets and no imposed look** — shapes,
transparency, your own textures, and text are the vocabulary, widgets are
things you build and save yourself (§3, §5); `find()`'s cost is real and gets
the three-layer fix (§8.1).

**Still open:**

1. **Default layer resolution** — `native` (crisp, readable; retro is one
   click) or `retro` (maximum aesthetic commitment)? *Proposal: native.*
2. **Widget = sub-scene** (§3) — comfortable with widgets being `.ron` scenes
   + a script with `defaults`, or do you want a dedicated widget asset type?
   *Proposal: sub-scenes; zero new machinery.*
3. **`ui.make` table shape** (§8) — happy with the `{ "row", … }` literal
   style, or prefer builder calls (`ui.row{...}:add(ui.text{...})`)?
   *Proposal: the table literal — it diffs/copies/pastes cleanly.*
4. **Naming** — `Matter::Gui`/`Element` and the `ui.*` global, or a flavored
   name for the system? (It'll be in every tutorial forever.)

## 14. What this fixes, by engine (the receipts)

- **vs Unity UGUI**: no anchors/pivots (Stack/Pin), no prefab-nesting ritual
  (sub-scenes you already know), no serialized-event wiring (hooks by name),
  live edit without domain reload.
- **vs Unity UI Toolkit**: one authoring surface instead of UXML+USS+C#; data
  tokens instead of a stylesheet language; the visual editor is the *primary*
  tool, not a preview.
- **vs Roblox**: no UDim2 math (place freely in px/%, pin corners with a
  preset, opt into real auto-layout when you want it), Inspector-wired node
  refs + bindings instead of per-frame `FindFirstChild` and property
  assignment, and juice (state transitions) as data instead of TweenService
  boilerplate.
