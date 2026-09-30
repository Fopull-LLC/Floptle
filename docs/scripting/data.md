# Data, time & the outside world

Saved games, timers, HTTP, the player's account, and the settings a game offers.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [23. Saving: `save.set`, `save.get` & slots](#23-saving-saveset-saveget-slots)
- [24. Timers: `after`, `every` & `tween`](#24-timers-after-every-tween)
- [26. The web: `http.*` & `json.*`](#26-the-web-http-json)
- [27. The player's account: `account.*`](#27-the-players-account-account)
- [30. Settings a game offers its player: `app.*`](#30-settings-a-game-offers-its-player-app)

---

## 23. Saving: `save.set`, `save.get` & slots

Persistent game data — survives Play sessions, editor restarts, and installing a
new version of an exported game. One key→value store per **slot** (its own file
under `save/`). In the editor that is the project's `save/`; in an exported build
it is `save/` inside the player's own data folder, which outlives the build (see
[Where a player's data lives](../export-builds.md#where-a-players-data-lives)).

```lua
save.set("gold", save.get("gold", 0) + 10)
save.set("checkpoint", { scene = scene.current(), x = node.x, y = node.y, z = node.z })
save.flush()                       -- checkpoint NOW (else: auto on Stop + ~5 s)

local cp = save.get("checkpoint")
if cp then scene.load(cp.scene) end

save.slot("slot2")                 -- separate profile; save.slot() reads the name
```

Values follow the `synced`-var guardrails: numbers, strings, booleans, tables up
to depth 4 and ≤ 1 KB each — no functions/userdata. A violation is a script
error, not silent data loss. A slot holds at most **10 000 keys** and **4 MB**
in all; a `save.set` past either is the same kind of error.

**Multiplayer**: this is *local* storage. For server-authoritative progress,
call `save.*` inside server-side paths (`net.isServer()`) and hand results to
clients via `synced` vars or RPC.

---

## 24. Timers: `after`, `every` & `tween`

Schedule work in **game time** — tick-driven and deterministic (timers pause
with the game, fire at the same tick on every machine, and never drift with
frame rate). Callbacks get no arguments; capture what you need as locals.

```lua
after(2, function() door.visible = false end)      -- once, in 2 s

local beeper = every(1, function()                 -- repeatedly, every 1 s
  audio.play("sounds/beep.ogg")
end)
beeper:cancel()                                    -- stop it (handles all have :cancel())

local y0 = node.y                                  -- animate: alpha eases 0 → 1
tween(0.5, function(a) node.y = y0 + a * 3 end, "smooth")
```

* `after(seconds, fn) → handle` — fire once.
* `every(seconds, fn) → handle` — first fire after one period, then anchored
  repeats (a long session doesn't drift; a stall never bursts to catch up).
* `tween(seconds, fn [, ease]) → handle` — `fn(alpha)` every tick, the final
  call landing **exactly** at `1.0`. Eases: `"linear"` (default), `"smooth"`,
  `"in"`, `"out"`.

An error inside a callback logs to the Console and kills only that timer. On a
scene switch all pending timers drop (they belonged to the old scene). In a
networked session timers advance on the global tick only — prediction replays
can't double-fire them.

---

## 26. The web: `http.*` & `json.*`

Non-blocking requests to your own server, so a game can have an account, a card
list, a leaderboard or a shop. The callback runs on a later frame **on the main
thread**, so it is safe to touch nodes from it.

```lua
http.get(url [, opts], function(res) end)
http.post(url, body [, opts], function(res) end)   -- a TABLE body is sent as JSON
-- opts = { headers = {...}, timeout = 10, json = true }
-- res  = { ok, status, body, json, error }

json.encode(t)   json.decode(s)   -- decode returns nil, err rather than raising
openUrl(url)     -- open the player's own browser (the sign-in flow needs it)
```

The same calls work in a browser build, through the page's own `fetch`. There
the server has to allow the page's address (CORS), redirects are followed
rather than handed back, and a few headers are the browser's to set; see
[web-export.md](../web-export.md#networking-in-a-page).

**Bytes made small, and bytes as text: `data.*`.** A level, a ghost or a
picture saved inside JSON is smaller and survives the trip as text:

```lua
local packed = data.deflate(json.encode(level))          -- zlib, level 6
local text   = data.base64Encode(packed)                  -- safe inside JSON or a URL
local bytes, err = data.base64Decode(text)
local plain, err = data.inflate(bytes)                    -- nil, why for bad data
```

`data.deflate(bytes [, { level, format }])` takes `format = "zlib"` (the
default), `"raw"` or `"gzip"`, and `level` 0–9. `data.inflate` reads what
someone else sent, so bad data answers `nil, why` rather than raising, and it
stops at `maxSize` bytes (64 MB unless you say otherwise) so a small hostile
blob cannot expand to fill memory. `data.base64Encode(bytes, { url = true })`
uses the URL-safe alphabet; `data.base64Decode` reads either.

Play only; Stop and `scene.load` cancel everything in flight; a call from
`fixedUpdate` warns, because a reply's timing can never be replayed.

**[docs/web-api.md](../web-api.md) is the full page** — the `res` table in detail,
the device-code sign-in flow (`scripts/web_login.lua` in a new project), the rate limits,
and the one rule that makes an account-backed game possible at all:

> **The server decides what the player owns.** The client asks; it never
> announces. Anything a client can announce, a modified client can announce.

---

## 27. The player's account: `account.*`

`http.*` is your game talking to *your* server. `account.*` is your game talking
to **Floptle's** — Foverse accounts, Fobucks, cloud saves, leaderboards and
missions, on `fopull.com`.

```lua
account.signIn()          -- returns immediately; the player approves in a browser
account.state()           -- "signedOut" | "starting" | "waiting" | "signedIn" | "failed"
account.code()            -- while waiting: { code = "WXYZ-9999", url = "…", expiresIn }
account.player()          -- when signed in: { id, name, email, tier }
account.error()  account.cancel()  account.signOut()

account.get("/wallet", function(res) end)
account.post("/games/mygame/events", { event = "boss_killed", event_id = id }, cb)
account.put("/games/mygame/saves/slot1", { data = t }, cb)
```

The engine drives the OAuth device flow in Rust, because the provider mandates
PKCE S256 and Lua has no SHA-256 — so a script asks for a **player**, never a
token. There is no `account.token()` on purpose: a shipped game's Lua is
readable, and anything a script can hold, somebody can read out of the file.

The calls take a **path**, not a URL. One host, which is what makes attaching
the player's token to it safe.

The session lives in the OS keyring and is **shared with the Floptle Hub** —
sign in once, in whichever you opened.

**[docs/web-api.md](../web-api.md) § Floptle Cloud** is the full page: the mission
and wallet shapes, why the wallet is read-only, and the three answers that
surprise a first test (`event_id` is mandatory, an empty `awarded` is not always
a failure, and a mission pays nothing until it is approved).

### Reading as the game: `cloud.*`

Some things a game reads need no player at all: another player's profile
picture, the remote config you publish from the game page, a public
leaderboard on the title screen. `cloud.*` reads those as **the game**, with
the key in `project.ron` (⚙ Settings ▸ Networked). The engine attaches the key; a
script never handles it.

```lua
cloud.avatar(net.identity(peer).id, 64, function(tex, err)
  if not tex then return end            -- err is "no_picture" when they have none
  ui.make(row, { "image", w = 32, h = 32, texture = tex, radius = 16 })
end)
cloud.config(function(config, err) motd = config and config.motd end)
cloud.get("/games/" .. cloud.game() .. "/rank/laps:canyon", function(res) end)
```

| Call | What it does |
|---|---|
| `cloud.game()` | this project's game slug, or `nil` when it is not connected |
| `cloud.get(path, cb)` | a read under `/games/…` or `/players/…`; `res` as `http.get`, and a blob's bytes in `res.body` |
| `cloud.avatar(playerId [, size], cb)` | a player's picture as a texture, `cb(tex, err)`; size 64, 128 (default) or 256 |
| `cloud.config(cb)` | your remote config, `cb(config, err)` |

Reads only: a game key never writes, so saves, scores and uploads go through
`account.*` as the player. Play only, like `http.*`, with the same rate limits.

### Leaderboards, documents, files and counters by name

Four calls name a collection and hand back an object with the operations that
collection has. They build the paths, encode the names and unwrap the replies,
so a game writes no URL. Reads use the game key (or the player, for a private
collection); writes always go out as the signed-in player.

```lua
local times = cloud.rank("time:" .. level)
times:page({ limit = 25 }, function(page, err)      -- {entries, total, keep, next}
  if err then return show(err.message) end
  for _, e in ipairs(page.entries) do row(e.rank, e.player, e.value) end
end)
times:submit(61.25, { meta = { deaths = 2 }, blob = "replays/" .. runId }, function(r, err)
  if r and r.kept then print("new best, rank " .. r.rank) end
end)

local stages = cloud.docs("stages")
stages:put("canyon-2", data, { ifVersion = 0 }, function(doc, err)
  if err and err.code == "key_taken" then print("somebody already has that name") end
end)
cloud.blobs("replays"):put(runId, replayBytes)
cloud.counter("likes"):add("canyon-2", 1)
```

| Object | Operations |
|---|---|
| `cloud.rank(board)` | `:page{ limit, offset, around, player, sort }`, `:submit(value [, { meta, blob }])`, `:remove([playerId])` |
| `cloud.docs(name [, { private = true }])` | `:list{ prefix, owner, sort, after, limit }`, `:get(key)`, `:put(key, data [, { ifVersion }])`, `:delete(key)` |
| `cloud.blobs(name [, { private = true }])` | the same, with bytes (a string) where docs take a table; `:get` answers the bytes |
| `cloud.counter(name)` | `:add(key [, n])`, `:get(key)`, `:top{ prefix, sort, limit }` |

Every operation takes a last `function(result, err)`. `result` is the reply
the Cloud documents for that call. `err` is `nil` on success, and otherwise
the same table whichever call made it: `{ code = "key_taken", message = "...",
status = 409 }`. The codes are the Cloud's own (`no_such_collection`,
`private_collection`, `key_taken`, `version_conflict`, `invalid_blob`,
`storage_budget_exceeded`), or `http_<status>` and `network` when the server
said nothing more. A board's collection is the part of its name before the
first `:`, so `time:canyon` and `time:canyon:week39` both belong to `time`.

`floptle check` warns about a collection a script uses that
`cloud_collections.ron` does not declare. The one that matters most is a
ranking: an undeclared one is created on its first write keeping each player's
highest value, so a time trial would rank its slowest times first.

### Declaring what the game stores: `cloud_collections.ron`

Everything a game stores on fopull.com lives in a named collection. Public
data, leaderboards and counters have to be declared before the first write, or
the write is refused with `no_such_collection`. Keep the list with the game, in
`cloud_collections.ron` beside `project.ron`:

```ron
[
    (name: "replays", kind: blobs, access: public, cap_kb: 512),
    (name: "stages", kind: docs, access: public),
    (name: "time", kind: rank, keep: min),
    (name: "installs", kind: counter, mode: unique),
]
```

- **`kind`**: `docs` (JSON), `blobs` (bytes), `rank` (leaderboards) or `counter`.
- **`access`**: `private` (only the player who wrote it, the default for docs and
  blobs) or `public` (anyone playing can read it). Rank and counter collections
  are always public.
- **`keep`**, rank only, and required: `max` keeps each player's highest score,
  `min` their lowest (a lap time), `latest` their most recent.
- **`mode`**, counter only: `total` (the default) adds every count, `unique`
  counts each player once.
- **`cap_kb`**: the largest single object. Left out, the server's default holds.

```sh
floptle cloud collections path/to/game           # what differs; writes nothing
floptle cloud collections path/to/game --apply   # make fopull.com match
```

Without `--apply` it reads with the game key, so it needs nobody signed in and
fits in a build script: it exits 1 when the server differs from the file.
`--apply` declares as the developer signed in to the Hub, who has to own the
game. A collection that holds data can't change its `kind` or `access`, and the
server's refusal is printed as it is. A collection that is only on the server is
listed and left alone: deleting one deletes its data, so that stays a click on
the game's page.

---

## 30. Settings a game offers its player: `app.*`

Every game has a menu with **Quit** on it, and a Video tab, and an Audio tab.
Most of what those need has been here for a while and is documented elsewhere on
this page; `app.*` is the rest.

| | |
| --- | --- |
| `app.quit()` | end the game |
| `app.title()` | the game's title, for the top of a menu |
| `app.version()` | the engine version this build was made with |
| `app.platform()` | `"windows"`, `"macos"`, `"linux"` or `"web"` |
| `app.isWeb()` | `true` in a browser build, to hide what a page cannot do |
| `app.dataPath()` | the folder the player's `user://` files are in, for "your files are in …" (`nil` in a browser) |
| `app.vsync()` / `app.setVsync(mode)` | `"On"`, `"Adaptive"` or `"Off"` |
| `app.retro()` / `app.setRetro(on)` | the retro presentation — compositing small and upscaling |
| `app.retroHeight()` / `app.setRetroHeight(px)` | the height it composites at, for a pixel-art game |
| `app.renderScale()` / `app.setRenderScale(s)` | the fraction of the window the 3D scene renders at (0.25–1), upscaled smoothly with the UI kept sharp: the "render resolution" setting a weak GPU needs |
| `app.renderSharpness()` / `app.setRenderSharpness(s)` | how hard the upscale from a render scale below 1 sharpens, 0–1 (default 0.4; 0 is a plain stretch) |
| `app.retroIntegerScale()` / `app.setRetroIntegerScale(on)` | upscale by a whole number and letterbox, instead of stretching |
| `app.fullscreen()` / `app.setFullscreen(on)` | cover the screen (borderless, no mode switch), or go back to a window |

### Render scale is the setting that rescues a slow GPU

Pixel count is most of what a frame costs on the GPU. On one shipped level,
switching every lighting feature to its lowest setting saved 9% of the GPU
time, while `app.setRenderScale(0.67)` saved 44%. The 3D scene renders at that
fraction of the window and is upscaled smoothly. The game's UI draws
afterwards at full resolution, so text stays sharp. A Video menu usually offers
something like Native / 0.85 / 0.67 / 0.5 and keeps the player's choice with
`save.*`:

```lua
function start(node)
  app.setRenderScale(save.get("renderScale") or 1)
end
```

It's independent of retro mode. When retro is on, the retro height decides
the resolution instead.

The upscale sharpens by local contrast, so edges stay crisp while flat sky and
gradients do not turn to grain. `app.setRenderSharpness(s)` sets how hard,
from 0 (a plain bilinear stretch) to 1; the default is 0.4. A retro upscale is
hard pixels and is never sharpened.

Lines and rings a script draws with `draw.*` go into the scene, so they are
drawn at the lowered resolution too and go soft. `draw.nativeLines(true)` draws
them over the finished picture instead, one pixel wide at the window's own
resolution and in exactly the colour given, whatever the render scale; the
post effects (bloom, depth of field, grain) no longer touch them. An orbit map
or an aim reticle wants that.

Lines draw through the scene by default, so an orbit reads through its planet.
`draw.depthTest(true)` hides the lines queued after it behind whatever is in
front of them, native or not; switch it back with `draw.depthTest(false)` for
the ones that should stay on top:

```lua
draw.depthTest(true)
draw.ring(ground.x, ground.y, ground.z, 0, 1, 0, 0.5, 1, 1, 1)  -- hides behind the player
draw.depthTest(false)
draw.polyline(orbit, 0.6, 0.8, 1)                               -- reads through the planet
```

### What `app.quit()` does depends on where the game is running

There is one honest answer per host, and they are different things:

* **In an exported build** the game is the program, so it closes. Your `save.*`
  data is flushed first — somebody quitting from a settings menu expects the
  setting they just changed to have been kept.
* **In the editor** it stops Play. It is deliberately not a process exit: an
  editor that closed because a game under test called `quit` would take your
  unsaved work with it. A line in the Console says which of the two happened.
* **Under `floptle run`** the run ends where it stands, and the report says it
  stopped early rather than claiming it ran the whole span.

### Fullscreen is answered by the build even if your menu forgets

**F11** and **Alt+Enter** toggle it in every exported build, with no code on
your part — they are the two spellings of "fullscreen" a player will try
without reading anything. `app.fullscreen()` reports the real state, so a Video
tab that shows the setting stays right after the player used the key instead
of the menu.

In the editor `app.setFullscreen` leaves the window alone — it is the editor's
window, not the game's — and says so once in the Console, the same way
`app.quit()` is honest about where it is running.

### A setting you change is for this session only

Vsync and the retro settings live in `project.ron`, which is the file that ships
to everybody who plays. So changing one changes it **for this run**, and Stop
puts the project back exactly as it was — the same rule `audio.track(…)` follows.

Which means **persisting it is your game's job**, through `save.*`. That is not
an omission: a player's preference belongs in the player's save, not in a file
every player gets a copy of.

```lua
-- read them back on launch
function start(node)
  app.setVsync(save.get("vsync") or "On")
  app.setRetroHeight(save.get("pixelHeight") or app.retroHeight())
  access.setTextScale(save.get("textScale") or 1.0)
  audio.track("Master"):setVolume(save.get("masterDb") or 0)
end
```

A mode `setVsync` does not recognise is an **error**, not a shrug — a control
that silently keeps the old value is a control that appears to work. Same for a
`setRetroHeight` outside 32–4320.

### A settings screen, in full

The four tabs a player expects, and where each one comes from:

```lua
-- Audio — the project's mixer tracks (§11)
audio.track("Master"):setVolume(db)
audio.track("Music"):setVolume(db)

-- Video — the frame pacing and the internal resolution, plus the scene's
-- own post-processing, which is a component like any other (§7)
app.setVsync("Adaptive")
app.setRetroHeight(360)
local post = scene.find("Post Processing"):getComponent("PostProcess")
post.bloom = true          -- a flag
post.motionBlur = 0        -- an amount, 0..1 — the shutter, not a switch
-- the three players most often want off; each is an amount, and 0 is off
post.aberration = 0        -- chromatic aberration
post.distortion = 0        -- lens distortion (signed: negative pincushions)
post.grain = 0             -- film grain
-- "colour grade: off" puts the grade back to neutral: 0 for the offsets,
-- 1 for the scales
post.exposure, post.temperature, post.tint, post.lift = 0, 0, 0, 0
post.contrast, post.saturation, post.gradeGamma, post.gain = 1, 1, 1, 1

-- Accessibility — text scale, colour filter, reduced motion, captions
access.setTextScale(1.25)
access.setReducedMotion(true)

-- Controls — the action map, rebindable at runtime (§5)
for _, action in ipairs(input.actions()) do
  log(action .. ": " .. table.concat(input.bindingsOf(action), ", "))
end
input.startRebind("Jump")

-- …and the button that used to have nothing behind it
function onQuit()
  save.flush()
  app.quit()
end
```

A field a `PostProcess` does not have reads as `nil`, so a game can check for
one before it offers the toggle.

**Not here yet:** window resolution and monitor choice. Those are one
question — windowing — and it deserves answering properly rather than by adding
a resolution setter that only half works. `app.setRetroHeight` is the one that
already existed as a live render setting, and in a pixel-art game it is the
"resolution" a player means.
