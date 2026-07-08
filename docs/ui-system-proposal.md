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
2. **Layout by intent, not coordinates.** The default container *flows* its
   children (rows/columns, gap, padding, alignment). You say "these three
   buttons, stacked, 8 apart, centered" — never "y = 172". Pinning to screen
   corners is a preset, not arithmetic (kills P1).
3. **Direct manipulation.** You edit UI *in the Game view*, at game resolution,
   live — drag to reorder inside a stack, drag handles to resize, double-click
   a label to retype it. During Play too (kills P4).
4. **Tokens, not CSS.** One theme asset holds named colors/spacing/fonts;
   elements reference tokens; element states (hover/pressed/disabled) and their
   transition times are *data on the style*, not code. No selector language, no
   cascade debugging (kills P3, P6).
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
   │  ├─ HpBar              ← Element + Bar (a stock widget)
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
  - **Layout** — how it sizes itself and arranges children (§4). Every element
    has one (default: fit content).
  - **Style** — fill, border, corner radius, shadow, opacity, per-state
    overrides + transitions (§5). Optional (an invisible group needs none).
  - **Text** — content, font, size token, wrap, align.
  - **Image** — texture, 9-slice insets, tint (uses existing texture assets).
  - **Interact** — makes it clickable/hoverable/focusable; fires script hooks.
  - **Scripts** — the same additive script list every node has.
- **Widgets are sub-scenes.** A `Button`, `Slider`, `HealthBar` is a small
  `.ron` of elements + a script with `defaults` (the existing tunables
  machinery = widget properties in the Inspector). The engine ships a stock
  set; a team's design system is a folder of their own. No prefab system to
  build — scene spawning already exists.

## 4. Layout (the P1 killer)

Three modes on the Layout component, friendly vocabulary over a real flexbox
engine (the `taffy` crate — proven in production Rust UI):

- **Stack** *(the default for containers)* — `direction` (row/column), `gap`,
  `pad`, `align` (start/center/end/stretch), `justify` (start/center/end/
  space-between), `wrap`. Children flow; reordering is dragging in the
  hierarchy *or in the viewport*.
- **Pin** — for HUD placement: a 9-point preset (top-left … center … bottom-
  right) + an offset. This is the *only* place offsets exist, and they're two
  numbers, not four anchors and a pivot.
- **Free** — absolute within the parent, for the rare art-directed screen.

Sizing per element: `fixed(px)`, `pct(%)`, `fit` (content), `grow(weight)`
(share leftover space). That's the whole vocabulary — deliberately ~8 concepts,
not CSS's 40. Rule of thumb we design to: **90% of game UI is rows, columns,
gaps, and one pinned corner.**

## 5. Style & themes (the P3/P6 killer)

- **Theme asset** (`assets/ui/*.theme.ron`): named tokens —
  `colors.primary`, `colors.danger`, `space.s/m/l`, `radius.m`,
  `font.body/title` — plus the stock widgets' default looks. The project picks
  an active theme; swapping themes reskins the game.
- **Style component**: every visual property is either a literal or a token
  reference (the Inspector color picker has a "token" tab first — the pit of
  success is *using* the theme).
- **States on the style, not in code**: `hover`, `pressed`, `disabled`,
  `focus` blocks override base properties; a `transition` duration+easing per
  block animates the change. A juicy button — grow 4% on hover in 80 ms, dip
  on press — is **zero lines of Lua**.
- **No selectors, no cascade.** Inheritance is exactly: children inherit font
  and text color unless overridden. Everything else is explicit per element or
  comes from the widget's defaults. Predictability beats power here.

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
  option, pixel-perfect in `retro` layers. The engine ships one stock bitmap
  font + one clean TTF.
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
function start()
    ui.bind(find("HpBar"),  "value", function() return synced.hp / synced.maxHp end)
    ui.bind(find("Coins"),  "text",  function() return ("%d ¢"):format(synced.coins) end)
    ui.bind(find("PauseMenu"), "visible", function() return input.pressedToggle("Escape") end)
end
```

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
special moments. Deliberately *no* imperative x/y positioning API — layout
stays layout's job.

## 9. The editor experience

- **Edit in the Game view.** Selecting a Gui layer switches the viewport into
  UI-edit: elements outline on hover, click selects (Inspector shows the
  modular components), drag moves within Free/Pin or *reorders* within a
  Stack (insertion caret, like dragging a browser tab), handles resize,
  double-click edits text inline. Esc/click-out returns to the 3D scene.
- **Add menu ⏵ UI** — layer, empty stack, and the stock widgets. Dropping a
  widget `.ron` from Assets into the hierarchy instantiates it (exact same
  gesture as models/scripts today).
- **Live during Play** — styles, layout, text all hot-apply (the play-snapshot
  restore on Stop applies as usual: play-time experiments revert unless saved).
- **Theme editor** — a small panel listing tokens with pickers; edits hit the
  whole screen live. (The Material Editor pattern, reused.)
- **UI gallery scene** — `--play`-able scene shipped with the engine showing
  every widget in the active theme; doubles as our test bed.

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
| 3 | Themes + tokens + state blocks + transitions; theme editor; stock Button/Bar/Toggle | juicy hoverable menu, zero code; theme swap reskins it |
| 4 | Lua: hooks, `onClick`, `ui.bind`, `ui.make`, visibility; IDE surfaces | pause menu + live HP bar over the parry dummy fight |
| 5 | Widgets-as-sub-scenes + Slider/List/Scroll + bitmap fonts + `retro` layers; safe areas | the multiplayer join screen, made *in the engine*, replacing F1 for a real host/join |
| 6 | Animation-timeline channels; TextField; polish (focus nav, gamepad) | a settings screen navigable by controller |

Phases 1–3 are the "designer-first" bet paying off *before* scripting even
enters; that ordering is on purpose.

## 13. Decisions for Ty

1. **Default layer resolution** — `native` (crisp, readable; retro is one
   click) or `retro` (maximum aesthetic commitment)? *Proposal: native.*
2. **Stock theme look** — the shipped default theme sets the tone for every
   Floptle screenshot. Want to art-direct it early (a "Floptle look" the way
   the PS1 render is one)?
3. **Widget = sub-scene** (§3) — comfortable with widgets being `.ron` scenes
   + a script with `defaults`, or do you want a dedicated widget asset type?
   *Proposal: sub-scenes; zero new machinery.*
4. **`ui.make` table shape** (§8) — happy with the `{ "row", … }` literal
   style, or prefer builder calls (`ui.row{...}:add(ui.text{...})`)?
   *Proposal: the table literal — it diffs/copies/pastes cleanly.*
5. **Naming** — `Matter::Gui`/`Element` and the `ui.*` global, or a flavored
   name for the system? (It'll be in every tutorial forever.)

## 14. What this fixes, by engine (the receipts)

- **vs Unity UGUI**: no anchors/pivots (Stack/Pin), no prefab-nesting ritual
  (sub-scenes you already know), no serialized-event wiring (hooks by name),
  live edit without domain reload.
- **vs Unity UI Toolkit**: one authoring surface instead of UXML+USS+C#; data
  tokens instead of a stylesheet language; the visual editor is the *primary*
  tool, not a preview.
- **vs Roblox**: no UDim2 math, real auto-layout by default, a theme system,
  bindings instead of per-frame property assignment, and juice (state
  transitions) as data instead of TweenService boilerplate.
