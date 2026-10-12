-- Talk to people. Attach to the Player.
--
-- Walk up to anything tagged "npc" and press Interact (E, or the pad's left
-- face button). What it says is data/<its name>.json:
--
--   { "start": "hello",
--     "lines": {
--       "hello": { "who": "Mira", "face": 0, "text": "Oh! Hello.", "next": "ask" },
--       "ask":   { "who": "Mira", "text": "Seen the well?",
--                  "choices": [ { "text": "Yes", "next": "yes" },
--                               { "text": "No",  "next": "no" } ] } } }
--
-- A line with neither "next" nor "choices" ends the conversation. "face"
-- picks a portrait from the portrait sheet; leave it out for no portrait.
--
-- This is a game script, not part of the engine: copy it, change it, or
-- write your own.

defaults = {
  --@desc How close to stand to someone to talk, in world units.
  reach = 1.6,
  --@desc Letters per second as a line types out.
  speed = 40,
  --@desc The 9-sliced box the words sit in.
  frame = "textures/dialogue_frame.png",
  --@desc A strip of portraits, one per "face".
  portraits = "textures/portraits.png",
  portraitCols = 2,
  font = "fonts/pixel.fnt",
  --@desc The font's own 10 pixels. The layer scales by whole pixels, so it
  --@desc stays crisp at any window size.
  textSize = 10,
}

local talk = nil -- the conversation in progress, or nil
local layer = nil -- the UI layer the box is built in

-- The nearest person in reach, or nil.
local function nearest(node)
  local best, bestD = nil, params.reach
  for _, npc in ipairs(findTagged("npc")) do
    local d = node:distanceTo(npc)
    if d < bestD then
      best, bestD = npc, d
    end
  end
  return best
end

local function text(s, extra)
  local t = { "text", text = s, font = params.font, textSize = params.textSize }
  for k, v in pairs(extra or {}) do
    t[k] = v
  end
  return t
end

-- Say goodbye: clear the box and give the player back their controls.
local function finish()
  talk = nil
  input.popContext("talk")
  ui.make(layer, {})
end

local function goTo(id)
  if not id or not talk.lines[id] then
    finish()
    return
  end
  talk.at, talk.typed, talk.focused = id, 0, false
end

-- Build the box for where the conversation is: the portrait, the name, the
-- words typed so far, and the choices once they have all typed out.
local function show()
  local l = talk.lines[talk.at]
  local done = talk.typed >= #l.text
  local words = {
    "col", key = "words", w = "grow", gap = 4, pad = 0,
    text(l.who or "", { key = "who", textColor = { 1, 0.85, 0.45, 1 } }),
    text(l.text:sub(1, math.floor(talk.typed)), { key = "say", wrap = true, w = "100%" }),
  }
  if done and l.choices then
    -- The focused choice is drawn in gold with a pointer; the rest plain.
    local f = ui.focused()
    local on = f and tonumber(f.name:match("^Choice(%d+)$")) or 1
    for i, c in ipairs(l.choices) do
      words[#words + 1] = {
        "button", key = "choice" .. i, name = "Choice" .. i, pad = 0, fill = { 0, 0, 0, 0 },
        onClicked = function() goTo(c.next) end,
        text((i == on and "> " or "  ") .. c.text,
          { textColor = i == on and { 1, 0.85, 0.45, 1 } or { 1, 1, 1, 0.75 } }),
      }
    end
  elseif done then
    words[#words + 1] = text("E", { key = "more", textColor = { 1, 1, 1, 0.6 } })
  end
  local box = {
    "row", key = "box", pin = "bottom", margin = 8, w = 300, h = "fit", minH = 76, pad = 8, gap = 8,
    texture = params.frame, slice = 0.34,
  }
  if l.face then
    box[#box + 1] = {
      "image", key = "face", texture = params.portraits, cols = params.portraitCols, rows = 1,
      cell = l.face, size = { 56, 56 },
    }
  end
  box[#box + 1] = words
  ui.make(layer, box)
  -- Choices are buttons, so arrows, the d-pad, Enter, a click or a tap all
  -- pick one. Focus the first once it exists: what ui.make describes this
  -- frame is built at the end of it.
  if done and l.choices and not talk.focused then
    local first = find("Choice1")
    if first then
      ui.focus(first)
      talk.focused = true
    end
  end
end

function start(node)
  layer = find("Dialogue")
end

function update(node, dt)
  if not layer then return end
  if not talk then
    local who = nearest(node)
    if who then
      ui.make(layer, text("E  talk to " .. who.name, { key = "hint", pin = "bottom", margin = 12 }))
    else
      ui.make(layer, {})
    end
    if who and input.justPressed("Interact") then
      local data, err = assets.readJson("data/" .. who.name .. ".json")
      if not data then
        log("nothing to say for " .. who.name .. ": " .. tostring(err))
        return
      end
      talk = { lines = data.lines, at = data.start, typed = 0 }
      -- A consuming layer swallows every other action, so the player stands
      -- still while talking without its own script knowing about this one.
      input.pushContext("talk", { consume = true, enabled = { "Interact" } })
      if talk.lines[talk.at] then
        show()
      else
        finish()
      end
    end
    -- The press that opened the conversation is not also the press that
    -- skips its first line.
    return
  end

  local l = talk.lines[talk.at]
  if talk.typed < #l.text then
    talk.typed = talk.typed + params.speed * dt
    -- A press on a half-typed line finishes it rather than skipping it.
    if input.justPressed("Interact") then
      talk.typed = #l.text
    end
  elseif l.choices then
    -- Interact picks the focused choice, the same as Enter or a click.
    if input.justPressed("Interact") then
      local f = ui.focused()
      local i = f and tonumber(f.name:match("^Choice(%d+)$")) or 1
      goTo(l.choices[i].next)
    end
  elseif input.justPressed("Interact") then
    goTo(l.next)
  end
  if talk then show() end
end
