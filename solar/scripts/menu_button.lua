-- The HUD's ☰ MENU button: checkpoint the save slot and return to the main
-- menu. Hidden during direct editor play (no slot active — nothing to save,
-- nowhere to go back to).

local el, mgr

function start(node)
  el = node:getcomponent("UiElement")
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
  if el then el.opacity = 0.8 end
end

function hoverEnd(node)
  if el then el.opacity = 1.0 end
end
