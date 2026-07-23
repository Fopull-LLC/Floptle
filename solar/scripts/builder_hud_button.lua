-- SAVE / LAUNCH buttons in the builder HUD. `params.action`: 1 = save the
-- blueprint, 2 = save + launch to the pad.

defaults = { action = 1 }

local el, builder, idle

function start(node)
  el = node:getcomponent("UiElement")
end

local function ensureIdle()
  if idle or not el then return end
  idle = { el.fillR or 0.1, el.fillG or 0.14, el.fillB or 0.18, el.fillA or 0.92 }
end

local function setFill(r, g, b, a)
  if el then el.fillR = r; el.fillG = g; el.fillB = b; el.fillA = a end
end

local function hoverFill()
  if not idle then return end
  setFill(
    math.min(1.0, idle[1] * 1.5 + 0.08),
    math.min(1.0, idle[2] * 1.45 + 0.1),
    math.min(1.0, idle[3] * 1.4 + 0.1),
    math.min(1.0, idle[4] + 0.05)
  )
  if el then el.border = 1.8; el.opacity = 1.0 end
end

function clicked(node)
  if not builder then builder = findScript("builder") end
  if not builder then return end
  local a = math.floor(params.action)
  if a == 1 and builder.doSave then builder.doSave() end
  if a == 2 and builder.doLaunch then builder.doLaunch() end
end

function hoverStart(node)
  ensureIdle()
  hoverFill()
end

function hoverEnd(node)
  if idle then setFill(idle[1], idle[2], idle[3], idle[4]) end
  if el then el.border = 1.0; el.opacity = 1.0 end
end

function pressed(node)
  ensureIdle()
  if idle then setFill(idle[1] * 0.7, idle[2] * 0.7, idle[3] * 0.7, math.min(1.0, idle[4] + 0.05)) end
end

function released(node)
  hoverFill()
end
