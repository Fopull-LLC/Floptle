# Talking to a web API

*`http.*`, `json.*`, `openUrl` — new in v0.20.0.*

A game that can't reach a server can't have an account, a card list, a
leaderboard or a shop. This page is the whole of that surface, and the one rule
that makes any of it safe.

---

## The rule, first

> **The server decides what the player owns.**

The client **asks**: *what do I have? · may I buy this? · here is what I did.*
The client never **announces**: *I now have 900 coins · this card is mine · my
score is 4,000,000.*

Anything a client can announce, a *modified* client can announce, and there is
no clever way around that — not obfuscation, not a checksum, not a secret in the
binary. A shipped game's Lua is readable and its network traffic is visible.
The only thing that works is the server checking, so design every endpoint as a
request the server is entitled to refuse:

| Instead of | Send |
|---|---|
| `POST /me/coins { coins = 900 }` | `POST /shop/buy { item = "hat" }` — the server prices it, checks the balance, and answers |
| `POST /me/score { score = 4000000 }` | `POST /runs { seed = 71, inputs = "…" }` — the server replays it, or at least sanity-checks it |
| `POST /me/cards { add = "dragon" }` | `POST /packs/open { pack = "starter" }` — the server rolls it and tells you what you got |

If an endpoint would let a player give themselves something by editing one
number, it is the wrong endpoint. That is true of every engine; Floptle just
can't hide it from you.

---

## The calls

```lua
http.get(url [, opts], function(res) end)
http.post(url, body [, opts], function(res) end)
http.put(url, body [, opts], function(res) end)
http.delete(url [, opts], function(res) end)
```

**`opts`** — all optional:

```lua
{
  headers = { Authorization = "Bearer " .. token },
  timeout = 10,     -- seconds, 0.1 … 120 (default 15)
  json = true,      -- force a parse even if the server didn't say so
}
```

**`res`** — what the callback receives:

| Field | |
|---|---|
| `res.ok` | a 2xx **and** nothing went wrong |
| `res.status` | the HTTP status (`0` if the request never got there) |
| `res.body` | the reply as a string, **always** — including on a 404 |
| `res.json` | the parsed body, when the server said JSON or you asked for it |
| `res.error` | what went wrong: a transport failure, a timeout, malformed JSON |

A 4xx or 5xx is **not** an error — it's an answer. `res.ok` is false, and
`res.body` still holds whatever the server said, because that is where an API
explains itself:

```lua
http.post(params.api .. "/shop/buy", { item = "hat" }, function(res)
  if res.ok then
    coins = res.json.coins          -- the server's number, not ours
  else
    log("the server said no: " .. res.body)
  end
end)
```

A **table body** is encoded as JSON for you. Pass a string when you need
something else (a form body, XML, a signed blob).

---

## Non-blocking, always

There is no blocking form, on purpose. The blocking form is the one everybody
reaches for, and it turns a 300 ms round trip into a 300 ms freeze.

The callback runs **on a later frame, on the main thread**, so it is safe to
touch nodes, spawn things, and call other scripts from inside it — none of the
usual worker-thread rules apply. What you cannot assume is *when*:

```lua
function start(node)
  http.get(url, function(res) ready = true end)
  -- `ready` is NOT true here. It will be true some frames from now.
end
```

Which is also why HTTP lives **outside the fixed tick**. A reply arrives when it
arrives, and no replay can reproduce that — so a call from `fixedUpdate` warns
once in the Console. Put it in `update`, `start`, an `every` timer, or an RPC
handler. In a rollback match, a decision that depends on a web reply will
diverge between peers; route it through the server with `net.rpc` instead.

---

## `json.*`

```lua
json.encode(value)          -- -> a JSON string
json.decode(s)              -- -> value, err
```

`decode` returns **`nil` and a message** on bad input rather than raising —
a reply from someone else's server is data, not a bug in your script:

```lua
local save, why = json.decode(text)
if not save then return log("corrupt save: " .. why) end
```

Two things worth knowing, both consequences of Lua having exactly one table
type:

- **A table with a `[1]` encodes as an array**, anything else as an object.
  `json.encode{}` is `{}`; `json.encode{1,2,3}` is `[1,2,3]`.
- **JSON `null` decodes to `nil`**, so a null field reads exactly like a missing
  one. Use `res.json.thing ~= nil` if you need to tell them apart, and prefer an
  API that omits rather than nulls.

---

## Signing a player in

`assets/scripts/web_login.lua` ships with the engine and is the worked example.
It uses the **device code** flow — the one a TV app uses, and the right one for
a game you distribute as a binary:

1. the game asks your site for a short pairing code
2. the player approves it in a **real browser**, on your site (`openUrl`)
3. the game polls until the code turns into a token
4. the token is kept, and every later request carries it

The game never sees a password, never stores one, and needs **no secret baked
into it** — which matters, because a shipped game's Lua is readable.

```lua
http.post(params.api .. "/auth/device", {}, function(res)
  code = res.json.user_code
  openUrl(res.json.verify_url)        -- the player approves it on your site
  poll = every(2, check)
end)

function check()
  http.post(params.api .. "/auth/device/poll", { code = code }, function(res)
    if res.json and res.json.token then
      poll:cancel()
      token = res.json.token
      loadInventory()
    end
  end)
end
```

The endpoints it expects — implement these, or change the script to match yours:

| Endpoint | Sends | Answers |
|---|---|---|
| `POST /auth/device` | `{}` | `{ user_code, verify_url, device_code? }` |
| `POST /auth/device/poll` | `{ code }` | `{ token }` once approved; anything else until then |
| `GET /me/cards` | — | `{ cards: [ … ] }`, with `Authorization: Bearer <token>` |

**Keeping the token.** `save.set("token", token)` survives a quit, which is what
makes "stay signed in" work. It is stored in the player's own save file in
plaintext — fine for a game token that your server can revoke, not fine for
anything reused elsewhere. Issue short-lived tokens and refresh them.

---

## Floptle Cloud (fopull.com) — `account.*`

Everything above is `http.*`: **your** game talking to **your** server. If the
server you want is Floptle's own — Foverse accounts, Fobucks, cloud saves,
leaderboards, missions — you do not use `http.*` at all. You use `account.*`,
and it is a smaller surface on purpose.

### Signing in

```lua
account.signIn()          -- returns immediately
account.state()           -- "signedOut" | "starting" | "waiting" | "signedIn" | "failed"
account.code()            -- while waiting: { code = "WXYZ-9999", url = "…", expiresIn = 900 }
account.player()          -- when signed in: { id, name, email, tier }
account.error()           -- why the last attempt failed
account.cancel()          -- they pressed Escape
account.signOut()
```

The engine drives the device flow **in Rust**, because the provider mandates
PKCE S256 and Lua has no SHA-256. That is not a limitation you work around —
it is the reason a script never has to think about any of this. Ask for a
sign-in, draw the code, and get a player.

**Polled, not called back.** Signing in takes as long as a person takes to pick
up their phone; a sign-in screen redraws every frame anyway. A whole account
screen is about twenty lines:

```lua
function update(dt)
  local state = account.state()
  if state == "signedOut" and input.pressed("enter") then account.signIn() end
  if state == "waiting" then
    local c = account.code()
    if c and c.code ~= shown then shown = c.code; openUrl(c.url) end
  end
end

function lateUpdate()
  local s, who = account.state(), account.player()
  if s == "signedIn" then
    draw.text(20, 20, "Hi, " .. who.name, 22, 1, 1, 1)
  elseif s == "waiting" then
    draw.text(20, 20, account.code().code, 40, 1, 1, 1)
  end
end
```

**One session, everywhere.** It lives in the OS keyring, shared with the Floptle
Hub — sign in from the Hub and your game already knows the player; sign in from
a game and the Hub does. Stop and `scene.load` drop pending callbacks and
abandon an unfinished sign-in, but never the session itself: nobody should have
to sign in again because they pressed Play twice.

### Calling the Cloud

```lua
account.get("/wallet", function(res) end)
account.post("/games/mygame/events", { … }, function(res) end)
account.put("/games/mygame/saves/slot1", { data = t }, function(res) end)
account.delete("/games/mygame/saves/slot1", function(res) end)
```

`res` is the same table `http.*` gives you, and the JSON is always parsed
because every Cloud endpoint answers JSON, including its errors.

**A path, not a URL** — and that is the whole security model. There is exactly
one host these can reach, which is what makes attaching the player's token to
them safe. A bare path gets the `/api/floptle/v1` prefix; `/userinfo` and
`/oauth/*` stay at the domain root where the contract pins them. A URL where a
path belongs raises at the call site, with the reason.

**There is no `account.token()`, and there will not be.** A shipped game's Lua
is readable — anything a script can hold, somebody can read out of the file and
post somewhere else. The token stays in Rust and is attached to requests there.

### The rule, again, because currency is where it bites

Floptle Cloud has **missions** and a **Fobucks wallet**, and Fobucks are real:
they buy real goods on fopull.com. So the wallet is **read-only** and there is
no route that credits it. A game reports what happened:

```lua
account.post("/games/mygame/events", {
  event    = "boss_killed",
  count    = 1,
  event_id = playerId .. ":boss:" .. n,   -- REQUIRED; makes the report idempotent
  meta     = { difficulty = "hard" },     -- stored, never load-bearing for an award
}, function(res)
  -- res.json.awarded is what the SERVER decided. It may be empty, and that is
  -- not always an error — see below.
end)
```

The server owns the rule that turns an event into money. A modified build can
lie about the event; it cannot invent the amount, claim a `once` mission twice,
or write the balance.

Three things that will surprise a first test:

* **`event_id` is mandatory** (`422 invalid_event_id`). Re-sending one returns
  `duplicate: true` and awards nothing — which is the point: a lost reply is
  safe to retry, and a captured request is not worth replaying.
* **`awarded: []` is not always a failure.** There is a shared daily Fobucks
  budget per account across every Floptle game. Once it is spent, an approved
  mission legitimately awards nothing. Check `duplicate` to tell the two apart.
* **A mission pays nothing until it is approved**, whoever defined it — defining
  is self-service, being paid is not. Read `reward_active` off the mission and
  never promise money that will not arrive. Read `reward.amount` too, rather
  than hard-coding it: it is a live economic number.

The `cloud` scope is what unlocks all of it; a token without it gets
`403 insufficient_scope`. The engine requests it.

The one wire detail worth stating, because it is the thing most likely to trip
a JSON layer: **request bodies must be JSON objects.** In Floptle they are —
`json.encode{}` is `{}`, not `[]`, and there is a live test asserting the server
accepts it — but if you build a body by appending to a list you will get
`400 invalid_body`. Use explicit string keys.

### The account limits

| | |
|---|---|
| **6** account calls in flight | past this, the call raises |
| **20 s** timeout | not configurable; one server, whose timeouts we know |
| **1 MB** reply | four times the largest legitimate answer (a 256 KB save) |

---

## Secrets

There is no safe place for an API key in a shipped game. If a script can read
it, so can anyone who owns a copy. The device-code flow above exists precisely
so the client needs no secret at all.

For **development** — a key you use while building, that never ships — put it in
a project-level file your build ignores and your `.gitignore` covers, and read
it with `save.get`/a string param rather than typing it into a script that ends
up in a screenshot.

---

## The limits, and why they exist

| | |
|---|---|
| **8** requests in flight | past this, calls fail fast with `res.error` |
| **20** requests started per second | same |
| **8 MB** response body | larger replies fail rather than allocate without bound |
| **120 s** maximum timeout | 15 s by default |

Hitting a cap is nearly always a request inside `update()` — sixty a second,
forever. The cap says so **once** in the Console and then stays quiet: a line
per refusal would bury the Console under the very loop that caused it, and
silence would leave you with a script that mysteriously half-works.

**Play only.** Edit mode never opens a socket — a script being edited must not
be able to hit a live endpoint because the Inspector happened to re-run it.
**Stop and `scene.load` cancel everything in flight**, and a reply that arrives
afterwards is dropped rather than delivered: its callback closes over nodes from
a scene that no longer exists.

```lua
http.inFlight()     -- how many are still waiting
http.cancelAll()    -- forget them all (Stop and scene.load do this for you)
```

---

## See also

- [`scripting.md`](scripting.md) — the rest of the scripting API
- [`multiplayer.md`](multiplayer.md) — `net.*`, for players talking to *each
  other*. `http` is for your game talking to *your server*; they solve different
  problems and both have a place in one game.
