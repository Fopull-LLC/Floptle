-- One SAVE-SLOT button on the main menu. Shows what lives in the slot (a new
-- galaxy, or the seed of the one you started); clicking selects the slot and
-- enters the game — game_manager takes it from there (loading screen, galaxy
-- regeneration from the slot's seed, saved-position restore).

defaults = { slot = 1 }

local el

-- Peek at another slot's store without losing the current one: save.slot()
-- switches the active store (flushing first), so read and switch back.
local function peek(name, key)
  local cur = save.slot()
  save.slot(name)
  local v = save.get(key)
  save.slot(cur)
  return v
end

function start(node)
  el = node:getcomponent("UiElement")
  local name = "slot" .. math.floor(params.slot)
  local seed = peek(name, "g_seed")
  if seed and seed > 0 then
    node.text = string.format("SLOT %d   ·   galaxy %d", params.slot, seed)
  else
    node.text = string.format("SLOT %d   ·   new galaxy", params.slot)
  end
end

function clicked(node)
  save.slot("slot" .. math.floor(params.slot))
  scene.load("system")
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
