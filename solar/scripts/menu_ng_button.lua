-- A button INSIDE the New Galaxy panel: forwards its click to menu_newsave's
-- press(role) router and handles its own hover tint. role: "create" /
-- "cancel" / "seed" (the seed row — clicking clears back to random).

defaults = { role = "cancel" }

local el

function start(node)
  el = node:getcomponent("UiElement")
end

function clicked(node)
  local mgr = findScript("menu_newsave")
  if mgr then mgr.press(params.role) end
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
