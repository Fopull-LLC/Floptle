# Editor scripting

Every `.lua` file in a package's `editor/` folder runs **in the editor** when the
project opens. It can add menus, panels, Scene-view overlays and world-space
handles; read and edit the scene with real undo; remember things between
sessions; and — if the package asked for it — talk to a server.

New to packages? Start at [packages.md](packages.md). This page is the reference.

```lua
local brush = 2.0

local panel = ed.window("Grass", function()
    gui.heading("Brush")
    brush = gui.slider(brush, 0.1, 20, "radius")
    if gui.button("Scatter here") then
        ed.undo()
        scene.create("Grass")
    end
end)

ed.menu("Grass/Brush…", function() panel:show() end)
ed.shortcut("Ctrl+G", function() panel:toggle() end)

ed.onSceneDraw(function()
    handles.color(0.3, 1.0, 0.5)
    handles.wireDisc(ed.camera().pos, vec3(0, 1, 0), brush)
end)
```

---

## Three rules worth knowing first

**Your script never touches the editor directly.** Reads come from a mirror of
the scene rebuilt once a frame; writes are queued and applied after the frame.
That is what makes it safe to edit the scene from inside a panel that is being
drawn.

**`gui.*` exists only while your panel is being drawn.** It is handed to the
function you gave `ed.window` / `ed.overlay` and taken away when that function
returns. Calling it from a timer or a menu item raises, with a message saying so.

**A package gets what it declared.** `http` and `sys` are simply absent unless
`package.ron` asks for them. So are `io`, `os` beyond the clock, `require` of
anything outside the package, and `load`/`dofile`.

---

## `ed` — the editor

### Panels, menus and shortcuts

| | |
| --- | --- |
| `ed.window(title, drawFn)` | a floating panel. Returns a handle |
| `ed.overlay(name, drawFn)` | a panel pinned inside the Scene view. Returns a handle; starts visible |
| `ed.menu(path, fn)` | a menu item. `"Grass/Brush…"` puts *Brush…* under a *Grass* menu |
| `ed.shortcut(keys, fn)` | `"Ctrl+L"`, `"Ctrl+Shift+F5"`. A bare letter is not accepted — the editor's own single-key bindings own the unmodified keyboard |

A panel handle answers `:show()`, `:hide()`, `:toggle()`, `:isOpen()` and — for a
window — `:focus()`. A window starts hidden; open it from your menu item.

### Hooks

| | |
| --- | --- |
| `ed.onUpdate(fn)` | every editor frame, before anything is drawn |
| `ed.onSceneDraw(fn)` | where `handles.*` works |
| `ed.onSceneOpen(fn)` / `ed.onSceneSave(fn)` | |
| `ed.onSelectionChange(fn)` | fires once per actual change |
| `ed.onPlay(fn)` / `ed.onStop(fn)` | before the scene changes under you |
| `ed.onUnload(fn)` | the project or the package is going away |

### Reading the editor

| | |
| --- | --- |
| `ed.project()` | `{ root, name, scene, engineVersion }` |
| `ed.camera()` | `{ pos, forward }` — the Scene view's camera |
| `ed.playing()` | |
| `ed.time()` / `ed.dt()` | seconds since the editor started, and this frame |
| `ed.package` | `{ id, name, version, root, path(rel) }` — your own package |

### Doing things

| | |
| --- | --- |
| `ed.undo()` | mark the edits that follow as one undo step |
| `ed.saveScene()` / `ed.openScene(rel)` | |
| `ed.play()` / `ed.stop()` | |
| `ed.message(title, body)` | a modal with an OK button |
| `ed.openUrl(url)` | `Browser` permission. `http://` and `https://` only |
| `ed.repaint()` | draw again promptly (for an animating panel) |
| `ed.log(…)` / `ed.warn(…)` / `ed.error(…)` | to the Console, tagged with your package's name. `print` does the same |

### Remembering things

Three stores, because "remember this" means three different things:

| | Scope | Lives in |
| --- | --- | --- |
| `ed.prefs` | this person, every project | the editor's config folder |
| `ed.store` | this project, everybody | `<project>/.floptle/packages/` |
| `ed.session` | until the editor quits | memory |

An API key belongs in `prefs`. A per-scene annotation belongs in `store`. A
"have I already asked?" flag belongs in `session`.

Each answers `get(key [, default])`, `set(key, value)` and `keys()`. Values are
strings, numbers and booleans; anything structured goes through `json.encode`
first, which keeps the files readable.

```lua
ed.prefs.set("apiKey", key)
local key = ed.prefs.get("apiKey", "")
```

### Files

| | |
| --- | --- |
| `ed.read(rel)` | text, or `nil`. Your own folder always; elsewhere in the project needs `Files` |
| `ed.exists(rel)` / `ed.list(rel)` | same rule |
| `ed.write(rel, text)` | project-relative. Always needs `Files` |

Nothing reaches outside the project, and nothing may climb out with `..`.

`require("lib/helper")` loads another Lua file **from your own package**, once,
into the same environment, and returns what it returned. Paths are relative to
the package root, not to `editor/`.

> **Keep your library files out of `editor/`.** Everything under `editor/` runs
> on its own when the package loads — that is what `editor/` means — so a module
> kept there is executed once as a script *and* again when something requires it.
> Put them beside it and require them by path:
>
> ```text
> my-package/
>   editor/main.lua      runs
>   lib/client.lua       required by main.lua as require("lib/client")
> ```

---

## `scene` — the node graph

Nodes are identified by a number. Reads come from this frame's mirror; writes are
queued and applied after the frame, so a value you set is visible on the *next*
one.

| | |
| --- | --- |
| `scene.all()` / `scene.roots()` | every node id / the top-level ones |
| `scene.find(name)` / `scene.findAll(name)` | names are not unique, and `findAll` says so |
| `scene.children(id)` | |
| `scene.info(id)` | everything about one node — see below |
| `scene.bounds(id)` | `{ min, max, center, radius }` in world space |
| `scene.raycast(origin, dir [, maxDist])` | `{ node, point, normal, distance }`, or `nil` |

`scene.info(id)` returns `{ id, name, kind, parent, children, pos, worldPos, rot,
scale, radius, extents, tags, layer, visible, scripts, asset }`. `pos` is local; `worldPos`
has the parents applied. `kind` is a stable name — `"mesh"`, `"camera"`,
`"pointLight"`, `"terrain"`, `"tilemap"`, `"empty"`… — that will not change
because a node type gained a field.

`extents` is the node's **oriented** half-extents in world units — read it with
`rot` when which way a thing faces matters, which is most of the time for a
placement or measurement tool. `bounds` is the world-aligned box around that same
oriented box, so a crate turned 45° reports a wider `bounds` than its `extents`,
correctly.

> A node with no measurable geometry — a folder, a light, a camera — has no
> `extents`, and its `bounds` falls back to its bounding sphere, which is loose
> on anything long and thin.

`scene.raycast` tests each node's oriented **box**, not its triangles — exact for
the built-in shapes, right to within its import bounds for a model, and wrong for
a doorway in a wall. It is enough for what tools do with a ray: find the ground
under a point, pick what is in front of the camera, snap to a surface.

```lua
local hit = scene.raycast(vec3(x, 100, z), vec3(0, -1, 0))
if hit then place(hit.point, hit.normal) end
```

Hidden nodes are not in the way, and a ray that starts inside something hits it
at distance 0.

Edits:

| | |
| --- | --- |
| `scene.setName(id, name)` | |
| `scene.setPos(id, v)` / `scene.setScale(id, v)` / `scene.setRot(id, x, y, z, w)` | |
| `scene.setVisible(id, on)` | |
| `scene.create(name [, parentId])` | an empty node |
| `scene.destroy(id)` | the node and its whole subtree |
| `scene.spawnPrefab(path [, at])` | |

`selection.get()`, `selection.active()`, `selection.set(ids)` and
`selection.clear()` do what they say.

---

## `handles` — drawing in the world

Queued from `ed.onSceneDraw` and painted over the Scene view. Immediate mode: the
list empties every frame, so a tool that stops drawing stops appearing.

```lua
handles.color(1, 0.6, 0.2)      -- 0–1 floats, inherited by everything after
handles.width(2)
handles.wireCube(centre, vec3(2, 2, 2))   -- size is the FULL extent
handles.label(centre, "spawn")
```

`color` · `width` · `line(a, b)` · `polyline(points [, closed])` ·
`poly(points)` (filled) · `wireCube(centre, size)` · `wireSphere(centre, r)` ·
`wireDisc(centre, normal, r)` · `arrow(from, to)` · `dot(at [, px])` ·
`label(at, text [, size])`.

Positions are `vec3(x, y, z)` or any `{x=, y=, z=}` or `{1, 2, 3}` table.

Handles paint *over* the scene rather than into it — an authoring aid hidden
behind the wall it is measuring is no use — and they are not drawn in the Game
view, which is meant to show what a player would see.

---

## `gui` — widgets

Only inside a draw callback. Widgets **return their new value**:

```lua
name    = gui.textField(name, "your name")
enabled = gui.checkbox(enabled, "enabled")
amount  = gui.slider(amount, 0, 100, "amount")
if gui.button("Go", "starts the thing") then go() end
```

**Text** — `label(text [, tip])` · `heading` · `small` · `monospace` ·
`colored(text, r, g, b [, a])` · `wrapped` · `link(text) → clicked`

**Buttons** — `button(text [, tip]) → clicked` · `smallButton` ·
`checkbox(value, text [, tip]) → value` · `toggle(value, text) → value` ·
`radio(selected, text) → clicked` · `selectable(selected, text) → clicked`

**Values** — `slider(value, min, max [, label])` · `drag(value [, speed [, label]])` ·
`textField(value [, hint [, grabKeyboard]])` · `passwordField(value)` ·
`textArea(value [, rows])` ·
`combo(label, options, index) → index` (1-based) · `colorEdit(r, g, b) → {r, g, b}`

**Layout** — `horizontal(fn)` · `vertical(fn)` · `group(fn)` · `indent(fn)` ·
`scroll(fn)` · `collapsing(title, fn)` · `enabled(on, fn)` · `width(px, fn)` ·
`height(px, fn)` · `separator()` · `space([px])` · `flexibleSpace()` (pushes
what follows to the far end of a row) · `available() → {w, h}`

**Feedback** — `progress(fraction [, text])` · `spinner()` ·
`helpBox(text [, "info" | "warn" | "error"])`

Pass `true` as `textField`'s third argument to take the keyboard this frame —
what a panel opened by a shortcut wants, so it can be typed into straight away.

**Painting**, for charts, heatmaps and anything there is no widget for.
Coordinates are pixels from the panel's top-left.

`rectFilled(x, y, w, h, r, g, b [, a [, round]])` ·
`rectOutline(x, y, w, h, r, g, b [, a [, px]])` ·
`line(x1, y1, x2, y2, r, g, b [, a [, px]])` ·
`circle(x, y, radius, r, g, b [, a])` ·
`textAt(x, y, text [, size [, r, g, b [, a]]])` ·
`reserve(w, h)` — claim space so the next widget does not draw over what you
painted, which is the call everybody forgets.

**Input** — `mouse() → {x, y, inside}` · `clicked()`

---

## `http` — talking to a server

Needs the `Network` permission. Always non-blocking: the callback runs on a later
frame, on the main thread, where the rest of this API is safe to use.

```lua
http.get(url, function(res)
    if res.ok then handle(res.body) else ed.error(res.error) end
end)

http.post(url, json.encode(body), { headers = { ["Content-Type"] = "application/json" } },
          function(res) … end)
```

`get` · `delete` · `post(url, body, …)` · `put` · `patch`. The optional `opts`
table takes `headers` and `timeout` (seconds). The reply is
`{ ok, status, body, error }` — a 4xx or 5xx is an *answer*, so you get the status
and the body with `ok` false rather than a transport error.

Eight requests may be in flight at once and a reply is capped at 8 MB.

### Signing in through a browser

```lua
local port = http.listen(0, function(req)
    ed.log("token: " .. (req.query.token or "?"))
end)
ed.openUrl("https://example.com/auth?redirect=http://127.0.0.1:" .. port)
```

`http.listen(port, fn)` binds **127.0.0.1 only** and returns the port actually
bound — pass `0` to let the machine pick a free one. It answers the first request
that arrives, hands your callback `{ path, query, body }`, and closes. It closes
itself anyway after five minutes, when the package reloads, and when the project
does. `http.stopListening()` closes it early.

---

## The rest of the environment

`vec3(x, y, z)` · `vec2(x, y)` — plain `{x=, y=, z=}` tables.

`json.encode(value)` / `json.decode(text)`.

`sys.openUrl(url)` and `sys.platform` — `Browser` permission.

`print` goes to the Console, tagged with your package's name, and prints tables
one level deep rather than `table: 0x…`.

Standard Lua: `assert` `error` `ipairs` `next` `pairs` `pcall` `xpcall` `select`
`type` `tostring` `tonumber` `rawget` `rawset` `rawequal` `rawlen`
`setmetatable` `getmetatable` `unpack`, plus `string`, `table`, `math`,
`coroutine`, `bit` (LuaJIT's, for hashing and packing), and `os.time` /
`os.clock` / `os.date` / `os.difftime`.

**There is no `_G`.** The environment is an allow-list, not a view of the real
globals, so there is nothing to reach through. To probe for something optional,
just read it — an unknown name is `nil`:

```lua
local bit = bit
if not bit then … end
```

---

## When something goes wrong

An error is reported to the Console **once** and the callback that raised stops
being called — a panel that raises every frame would otherwise fill the Console
faster than you can read it. The panel draws the error in place of its contents,
and the package's row in 📦 Packages carries it too.

Fix the file and press **⟲ Reload all**. A reload throws the whole Lua state away
and builds it again, so nothing survives it except `ed.prefs`, `ed.store` and
`ed.session` — and the panels you had open come back open.
