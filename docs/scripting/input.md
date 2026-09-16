# Input

Keyboard, mouse, and the action map.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [5. `input` — keyboard & mouse](#5-input-keyboard-mouse)

---

## 5. `input` — keyboard & mouse

Available while playing.

| Call | Returns |
|---|---|
| `input.key("w")` | `true` while the key is held |
| `input.pressed("space")` | `true` only on the frame it goes **down** (an edge) |
| `input.released("space")` | `true` only on the frame it goes **up** (an edge) |
| `input.typed()` | the **characters** entered this frame, as a string (see below) |
| `input.axis("a", "d")` | `-1` / `0` / `1` from a negative/positive key pair |
| `input.button(1)` | mouse button held (`0` left, `1` right, `2` middle) |
| `input.clicked(1)` | mouse button pressed this frame (an edge) |
| `local dx, dy = input.mouse_delta()` | mouse movement since last frame |
| `local x, y = input.mouse()` | cursor position, pixels |
| `input.scroll()` | wheel delta this frame |
| `input.setMouseLocked(true)` | pin + hide the cursor (FPS mouselook); `false` releases. Also `input.lockMouse()` / `input.unlockMouse()` |

### Getting the cursor back for your own menus

Clicking into the Game view pins the pointer there, so playing doesn't let the
mouse wander onto editor panels. The editor skips that if your game already has
something clickable on screen — a menu's buttons *are* the gameplay, and pinning
the pointer froze them dead.

That question is asked **every frame**, so a shop or a pause menu that opens two
minutes into a session takes the pointer back on its own; you don't have to do
anything, and the player doesn't have to know that Escape was an option.

`input.setMouseLocked(false)` also releases it explicitly, which is what to
reach for if your menu is drawn some way the editor can't recognise as
interactive (`draw.*`, a shader, your own hit-testing).

> Before **0.26.0** the question was only asked at the click that pinned the
> pointer. A game with no visible buttons during play — a twin-stick shooter,
> anything cursor-free — could never get the cursor back, and `setMouseLocked`
> did not release it either, because that is a separate lock owner. Its own
> menus were unclickable for the rest of the session.

### Escape always wins

While a game holds the cursor, **Escape gives it back to you** and the game
can't take it again until you click into the Game view. The Game view says so
along its bottom edge, in both directions — a grabbed cursor is invisible, so
the one thing that says how to get it back can't be the cursor.

That matters because a first-person camera calls `setMouseLocked(true)` from
`update`, on every single frame, which is the correct way to write one. Escape
does not fight that call: your game keeps asking, the editor keeps saying no,
and the moment you click back into the view your camera has the mouse again
with nothing to re-establish. Leaving the window does the same thing, so
alt-tabbing away and back lands you on a usable pointer rather than a grabbed
one.

While the editor is holding it, your game reads a **neutral mouse** — no
motion, no buttons, no wheel, and no action bound to any of them. Its keyboard
and gamepad are untouched, because it's still playing. So you can tune a value
in the Inspector mid-flight without the view spinning to follow the pointer and
without every click on a slider also firing your weapon.

> Before **0.40.1** Escape cleared the lock for exactly one frame and the next
> `update` took it straight back, so a first-person game in the editor had a
> cursor you could only recover by alt-tabbing out of the whole application.

Key names are **the same names the action map's key picker shows** (Project
Settings ⏵ Input) — one list, so a key you can bind is a key you can poll:

- `a`–`z`, `0`–`9`, `f1`–`f12`
- `space` `enter` `escape` `tab` `backspace` `delete` `insert` `home` `end`
  `pageup` `pagedown`
- `shift` `ctrl` `alt` `super` `capslock` (left and right collapse onto one name)
- arrows `left` `right` `up` `down`
- `,` `.` `/` `;` `'` `` ` `` `[` `]` `\` `-` `=`
- numpad `num0`–`num9` `num+` `num-` `num*` `num/` `num.`

> Before **0.21.1** the raw-key half of that list stopped at the arrows, so
> `input.pressed("f9")` — and every numpad, bracket and navigation key — was
> permanently `false` while the *same* key bound fine in Settings. If you worked
> around it, the workaround is no longer needed.

#### Which keys reach the game

Every key in that list, with **three exceptions the editor keeps for itself**:

| key | what takes it |
|---|---|
| `f1` | Play / Stop |
| `f2` | Pause |
| `f3` | Step one tick (Shift+F3 steps back) |

Those are the transport controls, and a game that could take `f1` could stop you
stopping it — which is the one key you need when a script has gone wrong. Polling
one of them writes a Console line the first time, and the Scripting tab lints it
where you wrote it, so it is never a mystery.

Everything else is the game's, **`tab` included**. That is worth stating because
until **0.33.0** it was not: egui gave Tab to the editor's own focus traversal
before the game saw it, so a press cycled the editor's panels and
`input.pressed("tab")` returned `false`. Tab is *the* convention
for opening an inventory — Minecraft, Terraria, Valheim, Don't Starve — so it is
the first key both a player and a developer reach for, and the failure had no
symptom: `false` is exactly what a key nobody pressed looks like. A game shipped a
bag on Tab, passed its headless tests, and heard about it from a player. It also
worked in an exported build, so the binding looked broken for the whole time you
were making the game and correct only after you stopped testing it. A focused Game
view now claims the keyboard in the editor and in a build alike.

A locked cursor is genuinely pinned to the window center (hardware lock where
the OS supports it, per-frame re-centering where it doesn't) — read motion with
`input.mouse_delta()`. Stop always releases the lock.

**`input.pressed` is a key; `input.typed` is a character.** `input.pressed("q")`
asks about the *physical* key where Q sits on a QWERTY board — on AZERTY that
key types `a`, and nothing in the name says so. `input.typed()` returns what
the player meant to write, resolved by the OS layout, with a paste (Ctrl/Cmd-V)
folded into the same string. It never contains control characters: Enter and
Backspace stay actions.

```lua
code = code .. input.typed()
if input.pressed("backspace") then code = code:sub(1, -2) end
```

Building a string by polling `a`–`z` gets the alphabet wrong for anyone whose
keyboard isn't yours, and gets it wrong for digits and punctuation on every
keyboard. For anything more than a few characters, use a **UI text field** —
it brings a caret, selection, the clipboard and key repeat with it
([ui-navigation.md](../ui-navigation.md)). `input.typed()` is empty while a field
has focus, because the field consumed them.

### Gamepads a script can actually see

Every other call here answers the *resolved* question — did this player press
Jump. None of them can answer the one underneath it: **is there a controller
here at all**. So "the pad was never enumerated", "it went into a slot the map
doesn't bind", "the window hasn't got focus" and "something downstream ate it"
all reach you as the same observation — nothing happens — and the bug report
that comes back is "controllers don't work".

| Call | Returns |
|---|---|
| `input.pads()` | a list of `{ index, name, connected }`, index 1-based |
| `input.padCount()` | how many pads are connected right now |
| `input.padButton(1, "South")` | that pad's button, **raw** — no action binding involved |
| `input.padAxis(1, "LeftStickX")` | that pad's axis, raw, −1..1 (triggers 0..1) |

```lua
for _, p in ipairs(input.pads()) do
  log(string.format("pad %d: %s%s", p.index, p.name, p.connected and "" or " (gone)"))
end
```

If the list is empty it is a *device* problem and nothing about your input map
matters. If the pad is listed but `input.action(...)` stays false, the pad is
fine and the **binding** is where to look — and `input.padButton` will tell you
the button is physically down while the action is not firing. Put that on a
controls screen and a player can diagnose it for you.

The list follows hot-plug: poll it, and a disconnected pad reads neutral rather
than freezing its last pose. Button and axis names are the variant names
(`South`, `East`, `LeftBumper`, `Start`, `LeftStickX`, `RightZ`, …), matched
case-insensitively; an unknown name reads `false`/`0` rather than erroring.

### Two local players on one axis

A binding can be scoped to a local player slot, which is what lets one `Move`
axis carry WASD for player 1 and the arrow keys for player 2 — and one pad per
player:

```ron
Stick( player: Some(0), id: Slot(0), x: LeftStickX, y: LeftStickY, deadzone: 0.25 ),
Stick( player: Some(1), id: Slot(1), x: LeftStickX, y: LeftStickY, deadzone: 0.25 ),
```

`player` is available on every binding form — actions, `Keys`, `Stick` and
`Analog`. **Without it two pads do not mean two players**: `Slot(n)` names a
*device*, not a player, so an unscoped pair contributes both sticks to both
players and the harder push drives both characters.

A player-scoped `id: Any` means *that player's own pad, or nothing* — so a
second player with no pad reads zero instead of mirroring the first player's
stick. Unscoped `Any` keeps its old meaning: the resolving player's pad, else
the first connected one.

### The camera projection (`camera.*`)

Turn a world point into a screen pixel (and back) against the **active game
camera** — the pixels are in the same space `input.mouse()` reports, so you can
hover and click 3-D things you drew:

| call | returns |
| --- | --- |
| `camera.worldToScreen(x, y, z)` | `sx, sy, depth, onscreen` |
| `camera.screenToRay(sx, sy)` | `ox,oy,oz, dx,dy,dz` (a world ray from a pixel) |
| `camera.screenSize()` | `w, h` (game viewport, pixels) |
| `camera.screenRect()` | `x, y, w, h` — the viewport's rectangle in cursor space. Use this, not `screenSize`, to ask "is the cursor over the game view?": in the editor the view is a docked panel, so `input.mouse()` carries its offset. (An edge-pan camera that compares the cursor's x against the *width* slides away forever the moment the panel is not at x = 0.) |
| `camera.exists()` | `true` once a live game camera is being fed |

`onscreen` is `false` for points behind the camera or outside the frustum — skip
those. **Click-on-line picking** (how the solar map's maneuver nodes are placed):
sample a drawn line into points, `worldToScreen` each, and keep the nearest to
`input.mouse()` within a pixel threshold; create at that point on `input.clicked(0)`.

```lua
local mx, my = input.mouse()
local best, bd
for _, p in ipairs(orbit_points) do
  local sx, sy, _, on = camera.worldToScreen(p.x, p.y, p.z)
  if on then
    local d = (sx - mx) ^ 2 + (sy - my) ^ 2
    if not bd or d < bd then best, bd = p, d end
  end
end
if best and bd < 18 * 18 and input.clicked(0) then create_node_at(best) end
```

### Raycasting

`raycast(ox,oy,oz, dx,dy,dz, max [, ignore])` casts a ray against the world's
colliders (the terrain **and** any walkable mesh colliders) **and every physics
body** (players, crates) and returns a hit table or `nil`:

```lua
-- ground within 1.2 units below me?
local h = raycast(node.x, node.y, node.z, 0, -1, 0, 1.2)
if h then
  -- h.x, h.y, h.z   the hit point
  -- h.nx, h.ny, h.nz the surface normal there
  -- h.distance       how far the ray travelled
  -- h.node           the node that was hit — a body OR a piece of level
  -- h.material       which material slot of it, where that means anything
end
```

`h.node` tells you what you hit, whether that is a crate, another player or the
wall of a room: `h.node:getscript("combat")` reaches its scripts, and
`h.node:material()` its material. Your own node's body never blocks your rays,
and the optional `ignore` arg skips one more node's body — the orbit camera
passes the character it follows, so it never reads as a wall.

#### What surface did I hit? — `h.material`

One map mesh usually has several materials on it: a building is one node and its
brick, its grass and its floorboards are three of its **material slots**. So
`h.node:material()` answers *the node's* material, which for a level of any size
is one answer for a lot of different ground. `h.material` is the slot the ray
actually landed on — the name the level author typed, the one the Inspector
shows — which is what a footstep, an impact decal or a splash needs:

```lua
-- Pick a footstep from the floor rather than from a volume drawn over the room.
local h = raycast(node.x, node.y, node.z, 0, -1, 0, 1.2, { layers = "Level" })
if h and h.material == "Boards" then
  audio.play("footstep_wood")
elseif h and h.material == "Grass" then
  audio.play("footstep_grass")
end
```

**It is `nil` for anything that has one surface**, and that is deliberate: a
terrain, a Collidable cube, an imported model, a physics body. A name invented
for those would be right for map meshes and quietly wrong everywhere else, which
is worse than no name. Branch on `h.material` and fall back to
`h.node:material()` when it is `nil`.

**It costs nothing until you read it.** The slot is looked up when you ask,
not when the hit is built, so a line-of-sight ray that only reads `h.distance`
pays for none of it. Read it once into a local if you need it twice.

The last argument can instead be an **options table**, which also filters by
[layer](nodes.md#18-layers-tags):

```lua
-- only the ground can block this ray — other players/props never will
local h = raycast(x, y, z, 0, -1, 0, 2.0, { ignore = target, layers = { "Ground" } })
```

`layers` takes one name or an array (Project Settings → Layers) and filters
**both** static geometry and bodies; a misspelled layer name is an error, not a
silent miss.

Use it for ground checks, line-of-sight, shooting, or dropping objects onto a surface.
(The built-in `node.grounded` already does a robust contact check for the character;
raycast is the general-purpose tool for everything else.)

### Shape queries — `overlapSphere`, `spherecast`, `capsulecast`

A ray answers *what is along this line*. A melee swing, an explosion or a
"can I fit there" asks a different question — *what is inside this volume* — and
a fan of rays answers it badly: it misses anything thinner than the fan and
cannot tell you how deep the overlap was.

```lua
-- Everything within 2 m of the sword, deepest overlap first.
for _, hit in ipairs(overlapSphere(swordTip, 2.0, { layers = "Enemies" })) do
  combat.hurt(hit.node, 25)
end

-- A thrown rock: a swept sphere hits what a ray squeaks past.
local h = spherecast(node.pos, vel:normalized(), 0.4, 30, { layers = {"Ground","Props"} })

-- "Can I actually walk there", asked with the shape that will be walking.
local blocked = capsulecast(node.pos, moveDir, 0.4, 0.9, 1.5)
```

| call | result |
|---|---|
| `overlapSphere(center, radius [, opts])` | a **list** of hits, deepest overlap first (empty when nothing is inside) |
| `spherecast(origin, dir, radius, max [, opts])` | the first hit, or `nil` |
| `capsulecast(origin, dir, radius, halfHeight, max [, opts])` | the first hit, or `nil` |

Hits carry the same fields a `raycast` hit does — `x/y/z`, `nx/ny/nz`,
`distance`, `node` and [`material`](#what-surface-did-i-hit--hmaterial) — so a
script that handles one handles the others. For an overlap, `distance` is the
**penetration depth** rather than a travel distance. `opts` is the same table
`raycast` takes, and your own body is skipped for you.

These are cheap here for a structural reason: every collider already answers a
signed distance, so an overlap is one distance test and a swept sphere is the
ray march with the radius subtracted. Unlike a ray, they also see **sensors** —
a hitbox usually does want to know it swept a trigger volume.

### Debug gizmos

Draw one-frame debug shapes over the viewport straight from code. They show in
the **Scene view only** (the Game view stays clean — it's what the player would
see), and the viewport's gizmos toggle hides them all. Colors are optional
`0–1` floats (default green); everything is **immediate mode** — call it every
frame you want the shape visible.

| Call | Draws |
|---|---|
| `gizmo.line(x1,y1,z1, x2,y2,z2 [, r,g,b])` | a world-space line |
| `gizmo.ray(ox,oy,oz, dx,dy,dz [, len [, r,g,b]])` | origin + direction (with `len` the direction is normalized — mirrors `raycast`) |
| `gizmo.sphere(x,y,z [, radius [, r,g,b]])` | a wire sphere (trigger zones, blast radii) |
| `gizmo.point(x,y,z [, size [, r,g,b]])` | a small 3-axis cross (hit points, waypoints) |

```lua
-- visualize a ground probe: green when it hits, red when it misses
local h = raycast(node.x, node.y, node.z, 0, -1, 0, 1.5)
if h then
  gizmo.ray(node.x, node.y, node.z, 0, -1, 0, 1.5, 0.3, 1.0, 0.4)
  gizmo.point(h.x, h.y, h.z, 0.2)
else
  gizmo.ray(node.x, node.y, node.z, 0, -1, 0, 1.5, 1.0, 0.35, 0.3)
end
```

The bundled character controllers ship with exactly this: set their `debug_ray`
param to `1` in the Inspector and the ground-check probe draws itself.
