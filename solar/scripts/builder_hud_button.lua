-- SAVE / LAUNCH buttons in the builder HUD. `params.action`: 1 = save the
-- blueprint, 2 = save + launch to the pad.

defaults = { action = 1 }

local el, builder

function start(node)
  el = node:getcomponent("UiElement")
end

function clicked(node)
  if not builder then builder = findScript("builder") end
  if not builder then return end
  local a = math.floor(params.action)
  if a == 1 and builder.doSave then builder.doSave() end
  if a == 2 and builder.doLaunch then builder.doLaunch() end
end

function hoverStart(node)
  if el then el.opacity = 0.75 end
end

function hoverEnd(node)
  if el then el.opacity = 1.0 end
end
