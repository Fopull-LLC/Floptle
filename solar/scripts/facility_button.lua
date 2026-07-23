-- A button inside the facilities panel. `params.action`: 1 = the context action
-- (Open Builder / Return to Menu), 2 = close the panel. Routes into facility_menu.

defaults = { action = 1 }

local el, menu

function start(node)
  el = node:getcomponent("UiElement")
end

function clicked(node)
  if not menu then menu = findScript("facility_menu") end
  if not menu then return end
  local a = math.floor(params.action)
  if a == 1 and menu.action then menu.action() end
  if a == 2 and menu.close then menu.close() end
end

function hoverStart(node)
  if el then el.opacity = 0.78 end
end

function hoverEnd(node)
  if el then el.opacity = 1.0 end
end
