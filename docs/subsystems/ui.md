# In-game UI (`floptle-ui`)

The game's HUD/menu system — **not** the editor (that's egui, ADR-0004). One
promise: **arrange elements, then script what they do.** Drag a panel, anchor it,
attach a script. No CSS engine, no data-binding framework, no fifty-field
inspector — but real creative control over look and behavior.

> Reads on: [ADR-0003 Lua scripting](../decisions/0003-scripting-lua.md) ·
> [ADR-0005 ECS/Node facade](../decisions/0005-scene-model-ecs-node-hybrid.md) ·
> [ADR-0004 Editor](../decisions/0004-editor-egui.md).
> Sits beside: [`./scene-and-nodes.md`](./scene-and-nodes.md) (UI elements *are*
> nodes) · [`./input.md`](./input.md) (modal UI pushes an input context) ·
> [`./camera-and-dialogue.md`](./camera-and-dialogue.md) (dialogue is built on this).
> Where it runs: [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §3 (variable update, after VFX).
> Crate `floptle-ui` depends on `floptle-core`, `floptle-render`.

## UI elements are nodes

There is no separate UI scene model. A UI element is a **node** with a `UiRect`
component (ADR-0005): the same tree, the same scripts, the same RON, the same
hot-reload as everything else. A HUD is a subtree under a `UiRoot` (a canvas in
screen space); a 3D world-space panel is the *same* subtree parented to a world
node. This is the single biggest simplification in the design.

> **Screen vs world space (shipped).** The layer component (`UiLayer`) has a
> `space` field — `Screen` (default: a flat overlay that fills the window) or
> `World` (a flat panel living in the 3D scene at the layer node's transform,
> sized by `canvas_scale` world units per design unit). Flip it in the
> Inspector's UI Layer section, or from Lua via
> `getcomponent("UiLayer").worldSpace = 1`. A world-space panel is depth-tested
> into the scene (geometry occludes it) and its buttons/sliders are clickable
> via a camera ray → panel-plane hit-test. Move/rotate the layer node to place
> it. *Known gaps:* the interaction ray doesn't yet test scene depth (a panel
> behind a wall still catches clicks), and there's no per-pixel occlusion of
> input.

The element set is deliberately **lean** — six leaves plus the layout containers:

```
UiRoot ─┬─ Panel        // a rect: background, 9-slice, padding; the box you group in
        ├─ Text         // a string + style (the only text primitive)
        ├─ Image        // a sprite/atlas region; tiling honored (ARCHITECTURE §6)
        ├─ Button       // Panel + Text/Image that emits hover/press/click events
        ├─ Bar          // Progress/health/stamina: a fill 0..1 with a style
        ├─ Group        // invisible layout container (Stack/Flow) — no visuals
        └─ (any of the above nest freely)
```

Each element is one component on a node:

```rust
struct UiRect {                 // every UI node has exactly this for layout
    anchor: Anchor,             // which parent edge(s) it sticks to (9-point + stretch)
    pivot:  Vec2,               // its own reference point, 0..1 (0,0=TL .. 1,1=BR)
    offset: Rect,               // px (or %) from the anchored edges — the only "position"
    size:   SizeMode,           // Fixed(w,h) | HugContents | Stretch
    style:  StyleRef,           // named, reusable style (below)
}

enum Element { Panel(Panel), Text(Text), Image(Image), Button(Button), Bar(Bar), Group(Group) }
```

## Layout — anchor + pivot + offset, set by dragging

Three numbers position anything, resolution-independently. No top/left/right/
bottom/margin/min/max soup.

- **Anchor** — which point/edge of the *parent* the element is pinned to. A
  9-point grid (corners, edges, center) plus **stretch** anchors (pin two opposite
  edges → the element grows with the parent). Picked from a tiny 9-dot widget, not
  typed.
- **Pivot** — the element's *own* reference point (0..1). Anchor top-right + pivot
  top-right ⇒ the corner hugs the corner at any resolution.
- **Offset** — pixels (or %) from the anchored edges. This is the *only* place a
  position lives, and you usually set it by dragging the element on the canvas.

```
 parent rect                          anchor = TopRight, pivot = TopRight
 ┌──────────────────────────────●←┐   offset = (right:16, top:16)
 │                               ╲ │   → stays 16px from the top-right corner
 │                          ┌─────┐│      on 1080p, 1440p, ultrawide — no code
 │                          │score││
 │                          └─────┘│
 └─────────────────────────────────┘
```

**Containers** cover the 90% case without a flex engine:

```rust
enum Group {
    Stack { dir: Dir, gap: f32, align: Align },   // vertical/horizontal list
    Flow  { gap: f32, wrap: bool },               // wrap to next line when full
}
```

Stack a column of buttons, flow an inventory grid. Children still keep their own
anchor/pivot for fine nudges. A layout pass resolves the tree top-down each frame
in a **virtual canvas** (a fixed design resolution, e.g. 1920×1080) then scales to
the real surface — so the editor preview at one resolution matches every device.

## Styles — small, named, reusable

Style is **not** per-element property soup. A handful of fields, named once,
reused everywhere; override only what differs.

```rust
struct Style {
    name: String,               // "HudLabel", "PrimaryButton"
    font: FontRef, size: f32, color: Rgba,
    padding: Rect,
    background: Option<Fill>,    // solid | gradient | image
    nine_slice: Option<NineSlice>,  // borders that stretch, corners that don't
    states: ButtonStates,        // optional tint/offset for hover/press/disabled
}

struct NineSlice { image: AssetId, border: Rect }   // the only "frame" mechanism
```

9-slice is the one frame mechanism: a bordered image whose corners stay crisp
while edges/center stretch — that's how a panel skin scales to any size. Styles
live in `ui/styles.ron`; an element references one by name, so reskinning the
whole game is editing a few styles, not every node.

## Interactions — events to node scripts

Elements emit events; **node scripts subscribe**. No widget owns logic; logic
lives in the script attached to the element's node (ADR-0003, the same
`on_event` lifecycle as everything else, ARCHITECTURE §7).

```
hover ─▶ UiEvent::HoverEnter / HoverExit
press ─▶ UiEvent::Press / Release
click ─▶ UiEvent::Click            (press+release on same element)
focus ─▶ UiEvent::FocusGained / FocusLost   (gamepad/keyboard nav)
value ─▶ UiEvent::ValueChanged(f32)         (Bar/slider-likes)
```

Focus + navigation make controllers work for free: directional input (an
[`./input.md`](./input.md) axis) moves focus across focusable elements; the
focused element gets `Press` from a `UiSubmit` action. A modal menu **pushes the
`menu` input context** (Consume) so the world stops hearing input while it's up.

A tiny Lua handler on a Button's node:

```lua
-- attached to the "PlayButton" node
function on_event(ev)
    if ev.kind == "HoverEnter" then self.node:play_style("hover") end
    if ev.kind == "Click"      then ui:open("scenes/level1.scn") end
end
```

That's the whole model: arrange the button in the editor, write four lines for
what it does. The `ui` table also exposes `ui:get("ScoreText"):set_text(...)`,
`ui:get("HpBar"):set_value(0.5)`, `ui:show/hide(name)` for scripts that drive the
HUD from gameplay.

## Editor UX — drag-drop canvas

A dedicated **UI workspace** (egui, dark/retro theme — ADR-0004). Arrange, anchor,
script — in that order, fast.

```
 ┌─ Elements ─┐┌─ Canvas  (1920×1080 ▾)  [▣ 1080p][▢ 1440p][▢ phone] ──────────┐┌─ Inspect ─┐
 │ ▸ HudRoot  ││                                                   ╔═══════╗   ││ Anchor    │
 │   ▸ TopBar ││   ┌──────────┐                       ●anchor dot  ║ ◔ ◔ ◔ ║   ││  ● ● ●    │
 │     Score  ││   │  SCORE   │                                    ║ ◔ ◉ ◔ ║   ││  ● ◉ ●    │
 │     HpBar  ││   │  1200    │           ▓▓▓▓▓▓░░░░  ← HpBar       ║ ◔ ◔ ◔ ║   ││  ● ● ●    │
 │   ▸ Menu   ││   └──────────┘                                    ╚═══════╝   ││ Style ▾   │
 │ [+ Panel]  ││            drag me · drag handles resize · snap to guides     ││ HudLabel  │
 │ [+ Text ]  ││                                                              ││ Script ▾  │
 │ [+ Button] ││                                                              ││ score.lua │
 └────────────┘└───────────────────────────────────────────────────────────────┘└───────────┘
```

- **Drag-and-drop**: drag elements from the palette onto the canvas; drag to
  move, handles to resize. Snapping to guides/siblings.
- **Anchor in two clicks**: select an element → the **9-point anchor picker**
  (the `◉` grid) sets anchor; drag the on-canvas **anchor dot** for off-grid /
  stretch. No anchor-field typing.
- **Inspector is small on purpose**: Anchor picker, a Style dropdown, a Script
  slot, and the offset/size if you want exact numbers. That's it — the whole
  point is *not* a hundred properties.
- **Live multi-resolution preview**: the resolution toggles re-render the canvas
  at 1080p / 1440p / phone so you *see* anchoring hold before shipping.
- Everything round-trips to RON; saving hot-reloads the running game.

## RON example — `ui/hud.scn`

A score readout (top-right) and a health bar (bottom-left), each a node with a
`UiRect` and a script.

```ron
UiRoot(
    design: (1920, 1080),
    children: [
        Node(
            name: "Score",
            rect: UiRect(
                anchor: TopRight, pivot: TopRight,
                offset: Rect(right: 24, top: 24),
                size: HugContents, style: "HudLabel",
            ),
            element: Text(value: "0"),
            script: "scripts/ui/score.lua",
        ),
        Node(
            name: "HpBar",
            rect: UiRect(
                anchor: BottomLeft, pivot: BottomLeft,
                offset: Rect(left: 24, bottom: 24),
                size: Fixed(280, 20), style: "BarGreen",
            ),
            element: Bar(value: 1.0, fill_image: "assets/ui/bar_fill.png"),
            script: "scripts/ui/hp.lua",
        ),
    ],
)
```

`score.lua` calls `self.node:set_text(state.score)` on `on_update`; `hp.lua` calls
`self.node:set_value(player.hp / player.hp_max)`. Arrange-and-script, end to end.

## Dialogue note

The **dialogue box** is a normal UI subtree (Panel + speaker `Text` + body `Text`
+ a continue glyph) built on exactly this system — themeable via a Style, no
special widget class. Its *runtime* (typewriter reveal, voice blips, advance/skip,
event weaving) is specified in
[`./camera-and-dialogue.md`](./camera-and-dialogue.md), which consumes input via
an [`./input.md`](./input.md) context.

## Out of scope

- **A full CSS/flexbox layout engine** — anchor + pivot + offset + Stack/Flow is
  the whole layout model on purpose. Complex grids are nested Stacks/Flows.
- **Data-binding / reactive frameworks** — scripts push values
  (`set_text`/`set_value`); we don't ship observable bindings or templating.
- **Rich text markup / inline-styled spans** beyond a single Style per Text, and
  a full **text-input/IME** stack — added only if a game needs them, not at launch.
- **Immediate-mode game UI** — `floptle-ui` is retained (nodes); use egui only in
  the editor (ADR-0004).
