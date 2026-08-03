-- The inventory panel's TRANSFER ALL button. The panel decides what "all" means
-- and where it goes; this is the click.

local el

function start(node)
  el = node:getcomponent("UiElement")
end

function clicked(node)
  local p = findScript("inventory_panel")
  if p and p.moveAll then p.moveAll() end
end

function hoverStart(node)
  if el then el.opacity = 0.85; el.border = 1.8 end
end

function hoverEnd(node)
  if el then el.opacity = 1.0; el.border = 1.2 end
end
