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

Persistent game data — survives Play sessions, editor restarts, and ships with
exported builds. One key→value store per **slot** (its own file under `save/`).

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

Play only; Stop and `scene.load` cancel everything in flight; a call from
`fixedUpdate` warns, because a reply's timing can never be replayed.

**[docs/web-api.md](../web-api.md) is the full page** — the `res` table in detail,
the device-code sign-in flow (`assets/scripts/web_login.lua`), the rate limits,
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
| `app.vsync()` / `app.setVsync(mode)` | `"On"`, `"Adaptive"` or `"Off"` |
| `app.retro()` / `app.setRetro(on)` | the retro presentation — compositing small and upscaling |
| `app.retroHeight()` / `app.setRetroHeight(px)` | the height it composites at: this engine's "resolution" |
| `app.retroIntegerScale()` / `app.setRetroIntegerScale(on)` | upscale by a whole number and letterbox, instead of stretching |
| `app.fullscreen()` / `app.setFullscreen(on)` | cover the screen (borderless, no mode switch), or go back to a window |

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
