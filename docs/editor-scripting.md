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
| `ed.tab(title, drawFn)` | a **dock tab**, dragged and split like the editor's own panels. Returns a handle |
| `ed.overlay(name, drawFn)` | a panel pinned inside the Scene view. Returns a handle; starts visible |
| `ed.overlay(name, options, drawFn)` | the same, placed and framed the way you ask |
| `ed.menu(path, fn)` | a menu item. `"Grass/Brush…"` puts *Brush…* under a *Grass* menu |
| `ed.pickFile(options, fn)` | the OS's file picker. `fn(paths)` gets a list, or `nil` if cancelled. Needs **Files** |
| `ed.shortcut(keys, fn)` | `"Ctrl+L"`, `"Ctrl+Shift+F5"`. A bare letter is not accepted — the editor's own single-key bindings own the unmodified keyboard |

A panel handle answers `:show()`, `:hide()`, `:toggle()`, `:isOpen()` and — for a
window — `:focus()`. A window starts hidden; open it from your menu item.

### Which of the three to reach for

They are not three styles of the same thing.

- **`ed.window`** floats above everything and can be moved anywhere. Right for
  something you call up, act on, and dismiss.
- **`ed.tab`** is handed to the dock. It can be dragged into any panel, split
  beside the viewport, stacked with the Inspector, and it comes back where you
  left it — across a reload of your package and across a restart of the editor.
  Right for anything somebody keeps open *beside* the scene rather than in front
  of it: settings, a list, a report.
- **`ed.overlay`** draws inside the Scene view itself, over the level. Right for
  something that is *about* what is on screen.

A tab starts closed, and where it sits is the user's arrangement — your package
can open and close it but cannot place it. The editor remembers the position by
your package id and the tab's title, so renaming a tab renames it in place;
changing your package's id gives it a new slot.

```lua
local settings = ed.tab("My Tool Settings", function()
    gui.label("Everything here docks like a normal panel.")
end)
ed.menu("My Tool/Settings…", function() settings:show() end)
```

A common pairing is an overlay that draws over the scene with a button that pops
the same content out into a tab — draw into a shared function, and call it from
both.

### Asking for a file

```lua
ed.pickFile({ title = "Choose an image", label = "Images",
              extensions = { "png", "jpg", "jpeg" } }, function(paths)
    if not paths then return end        -- cancelled
    local bytes = ed.read(paths[1])
end)
```

`multiple = true` allows more than one. The callback runs on a later frame, on
the main thread — the picker is the operating system's and takes as long as
somebody takes, so this is a request rather than a call that returns a path.
Cancelling gives `nil`, never an empty list: "no" and "nothing" read the same in
Lua and only one of them is true.

Absent entirely from a package that did not declare **Files**.

### Overlay options

| | |
| --- | --- |
| `corner` | `"topLeft"` or `"topRight"` (the default). The left stack starts **below the viewport toolbar**, wherever it has been dragged to |
| `bare` | `true` draws **no frame and no title** — for an overlay that paints its own |
| `width` | pixels, 40–900. Default 260 |

```lua
ed.overlay("My HUD", { corner = "topLeft", bare = true, width = 330 }, function()
    gui.rectFilled(0, 0, 320, 40, 0.05, 0.06, 0.08, 0.9, 5)
    gui.textAt(10, 12, "drawn, not themed", 13, 1, 1, 1, 1)
    gui.reserve(320, 40)
end)
```

Reach for `bare` when the overlay is a heads-up display rather than a panel: a
grey slab behind it hides the level the readout is about, which is the one thing
a Scene overlay must not do. If you take `bare`, you own the whole look — paint a
background behind anything you expect to be readable over a bright scene.

### Hooks

| | |
| --- | --- |
| `ed.onUpdate(fn)` | every editor frame, before anything is drawn |
| `ed.onSceneDraw(fn)` | where `handles.*` works |
| `ed.onSceneOpen(fn)` / `ed.onSceneSave(fn)` | |
| `ed.onSelectionChange(fn)` | fires once per actual change |
| `ed.onPlay(fn)` / `ed.onStop(fn)` | before the scene changes under you |
| `ed.onUnload(fn)` | the project or the package is going away |

### Waiting

| | |
| --- | --- |
| `ed.after(seconds, fn)` | run it once, later |
| `ed.every(seconds, fn)` | run it again and again |
| `t:cancel()` | stop one; both hand back a handle |

```lua
local poll
poll = ed.every(2, function()
    http.get(url .. "/progress/" .. id, function(r)
        if r.body.complete then poll:cancel() ; show(r.body) end
    end)
end)
```

They run on the **editor's clock** — the same one `ed.time()` answers with — so
nothing fires while the editor is not drawing, and a timer is not a way to
measure real time. What they are for is "in two seconds" and "every half
second".

A repeat keeps its period rather than drifting a frame at a time, and it never
catches up: a minute spent in a modal dialog costs you one firing, not a hundred
and twenty. A timer may cancel itself, or another, from inside its own callback.

Everything a package registers goes away on ⟲ Reload, timers included.

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
| `ed.lookAt(point [, distance])` | glide the Scene camera to a place — see below |
| `ed.openUrl(url)` | `Browser` permission. `http://` and `https://` only |
| `ed.repaint()` | draw again promptly (for an animating panel) |
| `ed.log(…)` / `ed.warn(…)` / `ed.error(…)` | to the Console, tagged with your package's name. `print` does the same |
| `ed.randomBytes(n)` | `n` bytes of real randomness, as a string (1–1024) |

> `math.random` is a generator seeded from the clock — right for a puff of smoke,
> wrong for anything somebody gets to guess at. Use `ed.randomBytes` for a
> sign-in challenge, a nonce, a token or an id that must not collide. Lua strings
> are byte strings, so the result is raw bytes and turning them into hex or
> base64url is yours to do.

### Taking somebody to a place

```lua
ed.lookAt(vec3(12, 0, -4))       -- ten metres back from there
ed.lookAt(hit.point, 3)          -- closer
```

The same move the `F` key makes — the view angle is kept and the camera glides
rather than jumping — aimed at a **point** instead of at the selection. Any tool
with a list of places in it needs this: a search result, a lint hit, a
measurement, a suggested position. The alternative is selecting a node in order
to move a camera, which is an edit to somebody's selection made behind their
back.

Nothing is selected, nothing is changed, and it does not care whether there is
anything at the point.

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
| `scene.setParent(id, parentId or nil)` | keeps the node where it is standing |
| `scene.create(name [, parentId])` | an empty node |
| `scene.destroy(id)` | the node and its whole subtree |
| `scene.spawnPrefab(path [, at])` | |

### Reading and writing the whole node

The setters above name one property each, and a tool that *builds* a level needs
the rest of them. `scene.set` and `scene.add` take the node **document** — the
same shape a `.ron` scene, a prefab and the clipboard all use — so anything a
node can be, a package can write.

```lua
-- Read one. `scene.doc` answers the whole node, in the same shape you write.
local doc = scene.doc(selection.active())
doc.name = doc.name .. " copy"
doc.transform.translation = { x, y, z }
scene.add(doc)          -- a real copy: its mesh, material, collider, tags
```

> **`scene.doc` reads a node in the selection.** That is a real limit and worth
> knowing rather than discovering: a node's document is every component it has,
> serialised, and building the whole scene's would mean rebuilding all of them on
> every frame of a gizmo drag in any project that had such a package installed.
> The selection is what a tool acts on and it is a handful of nodes.
>
> Asking for one that is not selected **raises**, saying so, rather than
> answering nil — a read that quietly returns nothing is how a tool places an
> empty node and reports success. `selection.set({id})` first, or use
> `scene.info(id)` for the summary, which is always available for every node.

```lua
-- Change what you name, and nothing else.
scene.set(id, { tags = {"cover", "movable"}, layer = "props" })
scene.set(id, { rigidbody = { mode = "Static" }, collidable = true })
scene.set(id, { transform = { translation = {4, 0, 2} } })   -- keeps the rotation

-- Build a room and everything in it: one call, one undo step.
ed.undo()
scene.add({
    name = "Guard Post",
    transform = { translation = {12, 0, -8} },
    children = {
        { name = "Crate", matter = { Primitive = { shape = "Cube", color = {0.6,0.5,0.4} } },
          tags = {"cover"}, collidable = true },
        { name = "Lamp",  matter = { PointLight = { color = {1,0.9,0.7}, intensity = 8 } } },
    },
})
```

**`scene.set` is a patch.** Only the keys you name change — which is what lets a
tool tint a light without knowing what else that light is, and what stops a tool
written for 0.64 from silently clearing a field 0.70 adds. Nested keys merge one
level, so naming `transform.translation` leaves the rotation and scale alone; a
list is a value, so `tags = {"cover"}` sets the tags to exactly that.

**A key that is not a node property is refused and named**, with the property you
probably meant. Nothing is half-applied: a typo three nodes down inside a
`children` list costs a Console line, not half a room.

**The node keeps its id.** An id you took last frame still names the same node
after a write, and its children stay under it.

`scene.info(id)` is the quickest way to see what a node of some kind carries —
the document uses the same names, and every field of it is in `docs/scripting.md`
alongside the node types themselves.

> Four keys are the scene *file's* and not yours: `id`, `parent_id`, `parent`
> and `attachment`. They link nodes together by position in a list, and a package
> re-pointing one wires the scene to something else without warning. Use the ids
> `scene.*` gives you, and `scene.setParent`.

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

## `nav` — the baked navmesh

Where a character can walk, as the level's bake describes it. `nav.ready()` is
`false` until somebody adds a **Nav Mesh** node and presses Bake, and every call
here answers `nil` until then — which is the ordinary state of a new project
rather than an error, so a tool that runs on every scene has to cope with it.

This is the reading half of the [scripting `nav`](scripting.md) API and nothing
that moves: no `nav.agent`, no `nav.obstacle`, no opening and closing links.
There is no simulation running for those to act on, and an obstacle carved into
the editor's own bake would be a level edit made by a panel.

Everything is in **world coordinates**, so a level a million units from the
origin needs no arithmetic at your end.

| | |
| --- | --- |
| `nav.ready()` | is there a bake to ask |
| `nav.settings()` | the character it was baked for: `radius`, `height`, `maxSlope`, `stepHeight`, `cellSize`, plus `area` in square metres |
| `nav.areas()` | the walkable surface — see below |
| `nav.links()` | the portals between those rectangles |
| `nav.ground()` | `{ {name, cost}… }`, the kinds of ground the level named |
| `nav.offLinks()` | the ladders, jumps and doors somebody placed |
| `nav.nearest(p [, max])` | the closest standable point, or `nil` |
| `nav.onMesh(p)` · `nav.regionOf(p)` · `nav.reachable(a, b)` | |
| `nav.path(a, b)` · `nav.distance(a, b)` · `nav.raycast(a, b)` | |
| `nav.random(seed [, near, radius])` | repeatable for a given seed |

### Reading the surface

`nav.areas()` hands back **one flat array of numbers and a count**, not an array
of tables. A real bake is thousands of rectangles, and one Lua table each
exhausts mlua's auxiliary slots and takes the editor down with it. The stride is
a constant so the arithmetic is written once:

```lua
local a, n = nav.areas()
local ground = nav.ground()
for i = 0, n - 1 do
    local o = i * nav.AREA_STRIDE
    local minX, minZ, maxX, maxZ = a[o+1], a[o+2], a[o+3], a[o+4]
    local yMin, yMax, region     = a[o+5], a[o+6], a[o+7]
    local cx, cy, cz             = a[o+8], a[o+9], a[o+10]
    local kind = ground[a[o+11]].name        -- "walkable", "water", …
end
```

`region` groups rectangles that can reach each other, so two with different
regions are two places you cannot walk between. `nav.LINK_STRIDE` does the same
job for `nav.links()`, whose entries are `from to leftX leftY leftZ rightX rightY
rightZ` — `from` and `to` index the areas array, one-based.

> **Two things are called links.** `nav.links()` is the thousands of portals the
> bake derived between neighbouring rectangles. `nav.offLinks()` is the handful
> an author placed by hand: `{ id, name, from, to, bidirectional, cost, duration,
> enabled, ground }`. The names are inherited and worth checking before you read
> either.

Points come back as vectors you can hand straight to `handles`:

```lua
ed.onSceneDraw(function()
    local a, n = nav.areas()
    handles.color(0.3, 0.8, 1, 0.5)
    for i = 0, n - 1 do
        local o = i * nav.AREA_STRIDE
        handles.wireCube(vec3(a[o+8], a[o+9], a[o+10]),
                         vec3(a[o+3] - a[o+1], 0.05, a[o+4] - a[o+2]))
    end
end)
```

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

**Type** — `font(name, fn)` · `hasFont(name) → boolean`. See
[Your own typeface](#your-own-typeface).

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
painted, which is the call everybody forgets. ·
`cursor() → {x, y}` — where the next widget would go, in these same
coordinates.

**Painting more than one thing needs `cursor()`.** The origin is the panel's
top-left and it does **not** move as widgets are added, so a second painted card
lands exactly on top of the first however much space you reserved between them.
Offset everything you paint by the cursor and a list works:

```lua
local at = gui.cursor()
gui.rectFilled(at.x, at.y, 200, 40, 0.05, 0.06, 0.08, 0.9, 4)
gui.textAt(at.x + 8, at.y + 12, "one row of many", 13, 1, 1, 1, 1)
gui.reserve(200, 40)   -- now the cursor has moved on
```

**Input** — `mouse() → {x, y, inside}` · `clicked()` ·
`keys() → {shift, ctrl, alt, enter, escape}`

`textField` returns **two** values: the text, and whether it was submitted with
Enter. With `keys()` that is enough to build the usual chat behaviour — Enter
for a newline, Shift+Enter to send, or the other way round:

```lua
local text, submitted = gui.textField(text, "Ask me anything…")
local k = gui.keys()
if submitted and (k.shift or k.ctrl) then send() end
```

Extra return values are dropped in Lua, so `x = gui.textField(x)` is unchanged.

### Your own typeface

A tool that arrives with a brand should be able to keep it. Ship the font in your
package and name it:

```ron
// package.ron
fonts: [ (name: "Heading", path: "fonts/YourFace-Black.ttf") ]
```

```lua
gui.font("Heading", function()
    gui.heading("Lumen")
    gui.small("LIGHTING TOOLS")
end)
```

`.ttf`, `.otf` and `.ttc`. The path is inside your package folder and cannot
leave it — which is why shipping a face needs **no permission**: reading a file
of your own is what `require` already does.

Everything drawn inside the closure uses that face, and **only the family
changes** — sizes are left alone, so `gui.heading` inside is still bigger than
`gui.label` inside. It applies to `monospace` too: a package that ships a mono
face and asks for it means it.

There is no `gui.setFont`. A face is chosen for the length of a closure, like
every other nesting call here, because a mode that outlives the panel that
switched it on is a mode somebody forgets to switch off.

> **The name is yours alone.** Two packages may both ship a `"Heading"` and
> neither sees the other's — names are scoped to the package that declared them.

A face that will not load is one line in the Console naming your package and the
file, and the panel draws in the editor's type. Never a row of tofu, and never
once a frame. `gui.hasFont(name)` is the same answer in advance, for a tool that
would rather draw its wordmark as an image than as the wrong type.

> A display face usually ships an alphabet and little else, so the editor's own
> stack sits behind yours as a fallback — a heading with an arrow or an emoji in
> it still draws.

---

## `mesh` — the triangles behind a node

Everything else here answers with a box. This answers with the geometry: what a
node is actually made of, or what is in a model file.

```lua
mesh.read(id, function(m, err)
    if not m then ed.warn(err) return end
    ed.log(m.vertices .. " vertices, " .. m.triangles .. " triangles")
end)

mesh.read("assets/models/chair.glb", function(m) … end)   -- a file, by path
```

A **callback**, like `http`, and for the same reason: the first read of a model
is a file off disk. Your callback runs on a later frame, on the main thread. A
read that cannot be answered still calls back — with `nil` and a reason, never
silently.

| | |
| --- | --- |
| `m.positions` | `{x, y, z, x, y, z, …}` |
| `m.normals` / `m.uvs` | the same shape, empty where the source has none |
| `m.indices` | triangle corners, **zero-based** |
| `m.vertices` / `m.triangles` | how many, so you can loop without dividing |
| `m.source` | `"model"`, `"map"` or `"primitive"` |

**Flat arrays, not a table per vertex.** A table each costs one of Lua's ~8000
registry slots and the editor *crashes* when they run out, so a hundred-thousand
vertex model has to arrive like this. Walk it by index:

```lua
for t = 0, m.triangles - 1 do
    local a = m.indices[t * 3 + 1]          -- Lua is 1-based…
    local x = m.positions[a * 3 + 1]        -- …but an index is 0-based
end
```

> **Indices are zero-based** even though Lua's tables are not, because every
> mesh format and every consumer of one counts from zero. Converting would make
> `positions[indices[i] * 3 + 1]` wrong in a way nothing would report.

**Positions are in the node's own space**, which is what a mesh file holds and
what an exporter wants — `scene.info(id)` carries the transform to place them
with. Returning world space would bake the current transform into data you might
be about to save.

No permission is needed: it reads what is in the scene, which the same package
can already measure and draw. Reading a file *outside* your package still needs
`Files`.

Terrain has no fixed triangles — it is meshed per chunk at the detail it is
viewed at — so `mesh.read` says so rather than picking a level of detail for
you. Sample it with `scene.raycast`. `mesh.maxTriangles` is the ceiling on one
read.

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

### Streaming — a progress bar that moves

A long job on a server usually offers **Server-Sent Events**: one open
connection the server writes to as it goes, instead of you asking "done yet?"
every second.

```lua
local s = http.stream(url, { headers = { Authorization = key } },
    function(frame)          -- once per event
        if frame.event == "progress" then
            pct = json.decode(frame.data).pct
            ed.repaint()
        end
    end,
    function(res)            -- once, when the connection closes
        if not res.ok then pollInstead() end
    end)
```

Each `frame` is `{ event, data }`. `data` is the text the server sent — decode
it yourself, since a server may stream JSON, plain text or nothing at all. An
event with no name is `"message"`, which is what the protocol says it means.

**Comments and keepalives never reach you.** Servers hold a connection open
through proxies by sending `: keepalive` every few seconds; that is protocol, not
data, and a package should not have to know it exists.

The handle answers `s:cancel()` and `s:isOpen()`. A stream closes itself when the
server closes it, when the package reloads, when the project does, and after 90
seconds of complete silence — not even a keepalive — which is a dead connection
rather than a quiet one. Cancelling takes effect within about ten seconds on the
network side; your callbacks stop immediately.

> **`onEnd` is not "it worked".** It says the connection closed, and `res.ok`
> tells you whether it closed the way it meant to. A server that has no streaming
> endpoint answers with a status instead, which arrives as `res.ok == false` and
> `res.status == 404` — that is your cue to fall back to polling, and it is worth
> writing that fallback, because a stream is the thing most likely to be blocked
> by somebody's proxy.

Four streams may be open at once. If a server sends frames faster than the editor
draws, frames are dropped rather than queued without limit, and `res.error` says
how many when the stream ends.

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
