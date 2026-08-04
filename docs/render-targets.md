# Render targets — a camera as a texture

A camera with a **target name** draws the world into a live texture instead of
onto the screen. Anything that takes a texture takes it: a material, a UI image.

```lua
find('MapEye'):setCamera{ target = 'minimap', width = 256, height = 256, hz = 10 }
find('MinimapPanel'):setMaterial{ texture = 'rt:minimap', unlit = true }
```

That is the whole mechanism. The name you give the camera is **bare**
(`minimap`); the texture that carries its picture is that name with an `rt:`
prefix (`rt:minimap`). Writing `target = 'rt:minimap'` is an error rather than a
texture called `rt:rt:minimap` that resolves to nothing.

It works in the editor while you place the screen, in Play, and in an exported
build — it is one path, not an editor feature.

> Older notes in `engine-roadmap.md` and `solar-demo-plan.md` spelled this
> `rt://<name>`. That prefix has never existed; it is `rt:`.

## Size and rate are yours

| field | means | default |
|---|---|---|
| `width`, `height` | the texture's pixels, 8 – 4096 | 480 × 270 |
| `hz` | redraws per second; `0` = every frame | `0` |

**Both matter more than they look.** A render target is a whole extra render of
the scene. A 256×256 minimap at 10 Hz costs about a sixth of what the same
minimap cost when every target was 480×270 at 60 — and a cockpit screen the
player glances at does not need 60 either. `perf.cost('render')` shows what you
are spending; see [Where the frame went](scripting.md#28-where-the-frame-went-perf).

A target that has never drawn always draws immediately, however slow its `hz` —
a 1 Hz camera must not show a second of black on the frame it appears.

## The four things people want

**Minimap.** A camera above the player looking straight down, parented to them
so it follows, with a narrow `cullMask` so only the layers that belong on a map
render.

```lua
function start(node)
  find('MapEye'):setCamera{
    target = 'minimap', width = 256, height = 256, hz = 10,
    -- `cullMask` is a bitmask over the project's layers, bit i = layer i, so
    -- only what belongs on a map renders into it.
    cullMask = (1 << 0) | (1 << 2),
  }
end
```

**Mirror / portal.** A camera at the mirror's position, facing back the way the
mirror faces, on a quad wearing `rt:mirror`. Recursion is bounded by
construction: while a camera fills its own target, the surface wearing that
target is drawn *without* its feed for that one pass — the GPU forbids sampling
a texture it is writing, so a mirror pointed at itself is a mirror with one
blank frame in it, never a hang. Two mirrors facing each other is fine; each
shows the other's last frame.

**Security camera / cockpit screen.** The cheap case, and the one where `hz`
earns the most: a bank of four monitors at 5 Hz costs a third of one at 60.

```lua
find('CamA'):setCamera{ target = 'cctvA', width = 320, height = 180, hz = 5 }
find('MonitorA'):setMaterial{ texture = 'rt:cctvA', unlit = true, emissive = {0.4,0.5,0.4} }
```

`unlit = true` is usually right for a screen: a monitor emits its picture, so
lighting it again dims it in shadow.

**Scope.** A narrow-`fovY` camera on the weapon, its target worn by the scope's
UI image, shown only while aiming. Set `hz = 0` here — a scope is the one case
where the player is looking straight at it.

```lua
find('ScopeEye'):setCamera{ target = 'scope', width = 512, height = 512, fovY = 0.12 }
```

## Split-screen

Two cameras, two targets, two UI images side by side. Each player gets their own
camera, their own layer mask, and their own HUD on top:

```lua
function start(node)
  find('P1Eye'):setCamera{ target = 'p1', width = 640, height = 720 }
  find('P2Eye'):setCamera{ target = 'p2', width = 640, height = 720 }
  ui.make {
    { 'image', texture = 'rt:p1', pin = 'topLeft',  width = '50%', height = '100%' },
    { 'image', texture = 'rt:p2', pin = 'topRight', width = '50%', height = '100%' },
  }
end
```

This composites through the UI layer rather than through a second surface pass,
which costs one extra blit and buys something a viewport split does not: each
half is an ordinary UI element, so it can be inset, bordered, animated, or
shrunk into a corner for a rear-view mirror.

## Limits, and what they say

* **Eight live targets per scene.** The ninth is not drawn, and the Console says
  which name was dropped and why. Which eight survive is decided by name, not by
  the order the scene happens to be walked — so a scene does not start failing
  because a node was added somewhere else entirely.
* **One camera per name.** Two cameras claiming `mirror` would take turns
  writing one texture, so the picture would flicker between two viewpoints. The
  second is not drawn and the Console says so.
* **Resizing reallocates.** Changing `width`/`height` builds a new texture; do
  it on a transition, not every frame.
* A target's texture keeps its last picture on the frames it does not redraw.
  That is what makes a low `hz` cheap rather than flickery.

## Reading it back

`node:getcomponent('Camera')` reports `fovY`, `active`, `width`, `height` and `hz`, so a
UI can size itself to the texture it is actually getting. `width`, `height` and
`hz` are also live mirror fields — a game can drop a screen's rate while the
player is not looking at it:

```lua
local cam = find('CamA'):getcomponent('Camera')
cam.hz = onScreen and 10 or 1
```

## Authority

`active = true` makes a camera the one the game view renders from, and clears
every other camera's authority. Two active cameras is not a choice anybody made
— the view would render from whichever the scene walk reached first.
