-- The HUD's ☰ MENU button: checkpoint the save slot and return to the main
-- menu. Hidden during direct editor play (no slot active — nothing to save,
-- nowhere to go back to).

local el, mgr, idle

function start(node)
  el = node:getcomponent("UiElement")
end

local function ensureIdle()
  if idle or not el then return end
  idle = { el.fillR or 0.1, el.fillG or 0.14, el.fillB or 0.18, el.fillA or 0.88 }
end

local function setFill(r, g, b, a)
  if el then el.fillR = r; el.fillG = g; el.fillB = b; el.fillA = a end
end

function update(node, dt)
  if not el then return end
  if not mgr then mgr = findScript("game_manager") end
  local show = (mgr and not mgr.loading and save.slot() ~= "main") or false
  el.visible = show
end

function clicked(node)
  if mgr and mgr.exitToMenu then mgr.exitToMenu() end
end

function hoverStart(node)
  ensureIdle()
  if not idle then return end
  setFill(math.min(1.0, idle[1] * 1.5 + 0.08), math.min(1.0, idle[2] * 1.45 + 0.1),
          math.min(1.0, idle[3] * 1.4 + 0.12), math.min(1.0, idle[4] + 0.06))
  if el then el.border = 1.6 end
end

function hoverEnd(node)
  if idle then setFill(idle[1], idle[2], idle[3], idle[4]) end
  if el then el.border = 1.0 end
end
