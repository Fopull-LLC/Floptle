-- One SAVE-SLOT button on the main menu. Shows what lives in the slot (the
-- seed of a started galaxy, or "empty"); clicking an OCCUPIED slot enters the
-- game (game_manager takes it from there — loading screen, regeneration from
-- the slot's seed + creation parameters, saved-position restore); clicking an
-- EMPTY slot opens the NEW GALAXY panel (menu_newsave) to pick the seed and
-- generation parameters first.

defaults = { slot = 1 }

local el, me

-- Peek at another slot's store without losing the current one: save.slot()
-- switches the active store (flushing first), so read and switch back.
local function peek(name, key)
  local cur = save.slot()
  save.slot(name)
  local v = save.get(key)
  save.slot(cur)
  return v
end

local function refresh(node)
  local name = "slot" .. math.floor(params.slot)
  local seed = peek(name, "g_seed")
  if seed and seed > 0 then
    node.text = string.format("SLOT %d   ·   galaxy %d", params.slot, seed)
  else
    node.text = string.format("SLOT %d   ·   empty", params.slot)
  end
end

function start(node)
  el = node:getcomponent("UiElement")
  me = node
  refresh(node)
end

function clicked(node)
  local name = "slot" .. math.floor(params.slot)
  if peek(name, "g_seed") then
    save.slot(name)
    scene.load("system")
  else
    local panel = findScript("menu_newsave")
    if panel then panel.openFor(math.floor(params.slot)) end
  end
end

-- menu_delete calls this (node:getscript handle) after wiping the slot so the
-- label flips back to "empty" without a scene reload.
function relabel()
  if me then refresh(me) end
end

function hoverStart(node)
  if el then
    el.fillR, el.fillG, el.fillB, el.fillA = 0.18, 0.32, 0.45, 0.95
  end
end

function hoverEnd(node)
  if el then
    el.fillR, el.fillG, el.fillB, el.fillA = 0.10, 0.14, 0.18, 0.88
  end
end
