-- Sign a player in to your website, from a downloadable game.
--
-- This is the DEVICE CODE flow — the same one a TV app uses, and the right one
-- for a game you ship as a binary:
--
--   1. the game asks your site for a short pairing code
--   2. the player approves it in a real browser, on your site
--   3. the game polls until the code turns into a token
--   4. the token is kept, and every later request carries it
--
-- The game never sees a password, never stores one, and needs no secret baked
-- into it — which matters, because a shipped game's Lua is readable. Anything
-- you put in a script is public.
--
-- THE ONE RULE, and it is the whole of client-server trust:
--
--   THE SERVER DECIDES WHAT THE PLAYER OWNS.
--
-- The client ASKS ("what do I have?", "I would like to buy this"). It never
-- ANNOUNCES ("I now have 900 coins"). Anything a client can announce, a
-- modified client can announce, and there is no clever way around that — only
-- the server checking. Read docs/web-api.md before you design an economy.
--
-- SETUP: attach to any node, set `api` to your API's base URL in the Inspector,
-- press Play. It expects the three endpoints listed in docs/web-api.md — point
-- it at your own server, or change the paths to match one you already have.
--
-- NOT THE WAY TO REACH FLOPTLE CLOUD. Signing in to a *Foverse* account is this
-- same flow, but the real provider requires PKCE (S256) and Lua has no SHA-256,
-- so the engine drives that one itself in Rust and hands you `account.*`:
--
--     account.signIn()  account.state()  account.code()  account.player()
--     account.get("/wallet", function(res) end)
--
-- Four lines instead of this file, no token in your script, and the session is
-- shared with the Floptle Hub. See docs/web-api.md § Floptle Cloud.
--
-- This script remains the pattern for YOUR OWN server, which is what most games
-- want anyway — and it is the readable version of what `account.*` is doing.

defaults = {
  --@header Your API
  -- Base URL, no trailing slash. Everything below hangs off it.
  api = "https://example.com/api",
  --@header Polling
  -- How often to ask whether the player has approved the code yet.
  --@range 1 10 --@units s
  poll_every = 2.0,
  -- Give up after this long and let them start again.
  --@range 10 600 --@units s
  give_up_after = 180.0,
  -- Open the approval page automatically. Off = show the URL and let them.
  open_browser = true,
}

-- Public state, so a UI script can read it through a handle:
--   local login = findScript("web_login")
--   if login.state == "ready" then showInventory(login.cards) end
state = "idle" -- idle | starting | waiting | ready | failed
code = nil     -- the short code the player types in, while state == "waiting"
verifyUrl = nil
token = nil    -- the bearer token, once they have approved
cards = {}     -- whatever /me/cards returned
message = ""   -- something a HUD can print verbatim

local poll = nil
local waited = 0.0

-- One place to say a request failed, so every path reports the same way.
-- `res.error` is set for a transport failure OR a malformed JSON reply; a 4xx
-- or 5xx leaves `ok` false with the server's own body still in `res.body`,
-- which is where an API explains itself.
local function failed(what, res)
  state = "failed"
  message = what .. ": " .. (res and (res.error or ("HTTP " .. tostring(res.status))) or "?")
  log(message)
  if res and res.body ~= "" then log("  the server said: " .. res.body) end
  if poll then
    poll:cancel()
    poll = nil
  end
end

-- Step 4. Ask the server what this player has. Note the shape: we ASK.
function loadInventory()
  http.get(params.api .. "/me/cards", {
    headers = { Authorization = "Bearer " .. token },
  }, function(res)
    if not res.ok or not res.json then return failed("loading your cards", res) end
    cards = res.json.cards or {}
    state = "ready"
    message = "signed in — " .. #cards .. " cards"
    log(message)
  end)
end

-- Step 3. Has the player approved it yet? Runs on an `every` timer, so it stops
-- by itself the moment it succeeds.
function check()
  waited = waited + params.poll_every
  if waited > params.give_up_after then
    return failed("nobody approved the code in time", nil)
  end
  http.post(params.api .. "/auth/device/poll", { code = code }, function(res)
    -- "not yet" is the normal answer here, not a failure: the endpoint says so
    -- with a 4xx or an empty token until the player has clicked approve.
    if res.json and res.json.token then
      token = res.json.token
      poll:cancel()
      poll = nil
      message = "approved"
      log(message)
      loadInventory()
    elseif res.error then
      failed("polling", res) -- a transport failure IS worth stopping for
    end
  end)
end

-- Step 1 + 2. Ask for a code and send the player to approve it.
function signIn()
  if state == "starting" or state == "waiting" then return end
  state, waited, message = "starting", 0.0, "asking for a code…"
  -- A table body is sent as JSON, so there is no json.encode dance here. Pass a
  -- string instead when you need to send something else.
  http.post(params.api .. "/auth/device", {}, function(res)
    if not res.ok or not res.json then return failed("starting sign-in", res) end
    code = res.json.user_code
    verifyUrl = res.json.verify_url
    state = "waiting"
    message = "go to " .. tostring(verifyUrl) .. " and enter " .. tostring(code)
    log(message)
    if params.open_browser and verifyUrl then openUrl(verifyUrl) end
    -- Poll on a timer rather than in update(): once every couple of seconds is
    -- the whole job, and `every` cancels cleanly.
    poll = every(params.poll_every, check)
  end)
end

function start(node)
  -- Nothing happens until something asks. Call signIn() from a menu button:
  --   ui.on(find("Sign in"), "clicked", function() findScript("web_login").signIn() end)
  message = "press the sign-in button"
  log("web_login ready — api = " .. tostring(params.api))
  signIn()
end

-- A minimal HUD, so the example is watchable without building a UI tree first.
function update(node, dt)
  local vx, vy = camera.screenRect()
  draw.text(vx + 24, vy + 24, "sign-in: " .. state, 20, 1, 1, 1)
  if message ~= "" then
    draw.text(vx + 24, vy + 50, message, 15, 0.8, 0.85, 0.95)
  end
  -- A dot per card, so a successful load is visible at a glance.
  for i = 1, #cards do
    draw.circle(vx + 24 + (i - 1) * 18, vy + 84, 6, 0.4, 1.0, 0.6, 0.9)
  end
end
