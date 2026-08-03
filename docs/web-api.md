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
