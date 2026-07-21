-- One SAS-mode button on the ship HUD (KSP-style clickable autopilot cluster).
-- Clicking it engages that hold mode on the ship; it lights up green while that
-- mode is active and hides itself whenever you're not flying. The `mode` string
-- param selects which mode (off / stability / prograde / retrograde / normal /
-- antinormal / radialin / radialout / node) — set per-button in the scene.
--
-- Talks to ship_controller through its published state: reads `sas_mode` to know
-- which button is active, calls `setSAS(mode)` to switch. (Cross-script handle:
-- ship.setSAS(...) / ship.sas_mode — the manager pattern.)

defaults = { mode = "stability" }

local el, ship

local function ship_ref()
  -- A piloted BUILT vessel owns the cluster; otherwise the scout ship does.
  -- (Vessels spawn/despawn at runtime — fetch fresh, scan EVERY instance.)
  for _, v in ipairs(findScripts("vessel_controller")) do
    if v.piloting then return v end
  end
  if not ship or not ship.node or not ship.node.valid then
    ship = findScript("ship_controller")
  end
  return ship
end

function start(node)
  el = node:getcomponent("UiElement")
end

function update(node, dt)
  if not el then el = node:getcomponent("UiElement") end
  if not el then return end
  local s = ship_ref()
  -- Shown only while piloting (and not in the map, which owns the screen —
  -- the map flag lives on the SCOUT script whichever craft is flown).
  local sc = findScript("ship_controller")
  local show = (s and s.piloting and not (sc and sc.map_view)) or false
  el.visible = show
  if not show then return end
  -- Highlight the active mode; dim the rest.
  if s.sas_mode == params.mode then
    el.fillR, el.fillG, el.fillB, el.fillA = 0.22, 0.80, 0.45, 0.95
    el.textR, el.textG, el.textB = 0.04, 0.09, 0.05
  else
    el.fillR, el.fillG, el.fillB, el.fillA = 0.10, 0.14, 0.18, 0.82
    el.textR, el.textG, el.textB = 0.72, 0.86, 0.96
  end
end

function clicked(node)
  local s = ship_ref()
  if s and s.setSAS then s.setSAS(params.mode) end
end

function hoverStart(node)
  if el then el.opacity = 0.82 end
end

function hoverEnd(node)
  if el then el.opacity = 1.0 end
end
