-- One DEVICE row in the peripherals panel: a fitted peripheral (landing gear,
-- comms dish, solar panels…) with its live state, its keybind and a click that
-- toggles it. `slot` picks which entry of the vessel's device list this row
-- draws; rows past the end hide themselves so the stack collapses cleanly.
--
-- The button and the keybind are the SAME control — both go through the
-- controller's `setPeripheral`, so they can never drift out of step.

defaults = { slot = 1 }

local el, entry

function start(node)
  el = node:getcomponent("UiElement")
end

function update(node, dt)
  if not el then el = node:getcomponent("UiElement") end
  if not el then return end
  local panel = findScript("peripheral_panel")
  local v = panel and panel.pilotedVessel and panel.pilotedVessel()
  local list = v and v.peripherals and v.peripherals()
  local d = list and list[math.floor(params.slot)]
  entry = d
  el.visible = d ~= nil
  if not d then return end
  -- MOVING is the honest third state: a leg halfway down is neither up nor
  -- down, and pretending otherwise is how you land on a retracting gear.
  local moving = (d.on and d.anim < 0.99) or (not d.on and d.anim > 0.01)
  -- A thruster bus is ON/OFF and switches instantly; a folding leg has a real
  -- in-between. Only the ones that actually take time report EXTENDING.
  local state
  if moving and not d.instant then
    state = d.on and "EXTENDING" or "RETRACTING"
  else
    state = d.on and (d.verbOn or "DEPLOYED") or (d.verbOff or "STOWED")
  end
  local text = string.format("  %s  ×%d      %s   [%s]",
    d.label, d.count, state, string.upper(d.key or "?"))
  if node.text ~= text then node.text = text end
  if moving and not d.instant then
    el.fillR, el.fillG, el.fillB, el.fillA = 0.30, 0.26, 0.10, 0.92
    el.textR, el.textG, el.textB = 1.0, 0.86, 0.5
  elseif d.on then
    el.fillR, el.fillG, el.fillB, el.fillA = 0.10, 0.30, 0.19, 0.92
    el.textR, el.textG, el.textB = 0.62, 1.0, 0.78
  else
    el.fillR, el.fillG, el.fillB, el.fillA = 0.10, 0.14, 0.18, 0.9
    el.textR, el.textG, el.textB = 0.68, 0.8, 0.9
  end
end

function clicked(node)
  local panel = findScript("peripheral_panel")
  local v = panel and panel.pilotedVessel and panel.pilotedVessel()
  if v and v.setPeripheral and entry then v.setPeripheral(entry.id) end
end

function hoverStart(node)
  if el then el.opacity = 0.85; el.border = 1.6 end
end

function hoverEnd(node)
  if el then el.opacity = 1.0; el.border = 1.0 end
end
