-- PERIPHERALS PANEL — the ship's device console, opened with P while flying.
--
-- Everything bolted to the hull that the pilot can actuate lives here in one
-- place: landing gear, the comms dish, solar panels, and every docking port
-- with its own latch state and its own UNDOCK button. The panel is a GENERIC
-- browser, not a hand-wired list — it asks the piloted vessel for its fitted
-- devices (`peripherals()`) and its ports (`dockPorts()`) and draws whatever
-- comes back, so a new peripheral appears the moment it's in the controller's
-- DEVICES registry. No second place to keep in sync.
--
-- This script owns the panel ROOT: the P toggle, the visibility rule, and the
-- approach readout at the top. The rows are `peripheral_row` (devices) and
-- `dock_row` (ports + controls), each of which finds its own data by slot.

local el, open, title

-- The vessel whose console this is: the piloted BUILT vessel, if any. Vessels
-- spawn and despawn at runtime (a lander undocks into its own craft), so this
-- is re-fetched every frame across every instance — never cached.
function pilotedVessel()
  for _, v in ipairs(findScripts("vessel_controller")) do
    if v.piloting then return v end
  end
  return nil
end

function start(node)
  el = node:getcomponent("UiElement")
  open = false
end

function update(node, dt)
  if not el then el = node:getcomponent("UiElement") end
  if not el then return end
  local v = pilotedVessel()
  -- The map owns the screen while it's up (the map flag rides the scout script
  -- whichever craft is being flown) — same rule the SAS cluster follows.
  local sc = findScript("ship_controller")
  local flying = (v and v.piloting and not (sc and sc.map_view)) or false
  if flying and input.pressed("p") then open = not open end
  if not flying then open = false end
  el.visible = open
  if not open then return end

  if not title then title = find("Peripherals Title") end
  if not title then return end
  local d = v.dock
  local head = "PERIPHERALS"
  if d and d.target then
    -- An approach in progress replaces the title with the numbers you fly on:
    -- how far, how square, how far off the axis, and the closing rate. Anything
    -- inside the green bands below will latch.
    local t = d.target
    head = string.format(
      "APPROACH  %s\n%.2f m   align %d%%   lateral %.2f m\nclosing %+.2f m/s%s",
      t.name, t.range, math.floor(t.align * 100 + 0.5), t.lateral, t.closing,
      d.assist and "   ·   MAGNETIC ASSIST ON" or "")
  elseif d and d.latched and d.latched > 0 then
    head = string.format("PERIPHERALS   ·   %d port%s latched",
      d.latched, d.latched == 1 and "" or "s")
  end
  -- The company line: what you're flying is worth money, and the contract you
  -- signed is the reason you're up here. Both belong where the pilot is looking.
  local co = findScript("company")
  local mi = findScript("missions")
  local tail = {}
  -- What we're carrying, when we're carrying anything: a hold is a peripheral
  -- like any other, and its number is the reason for the trip.
  if v.holdLine then
    local hl = v.holdLine()
    if hl then tail[#tail + 1] = hl end
  end
  if co and co.money then tail[#tail + 1] = co.money(co.balance()) end
  if mi and mi.activeLine then
    local l = mi.activeLine()
    if l then tail[#tail + 1] = l end
  end
  if #tail > 0 then head = head .. "\n" .. table.concat(tail, "   ·   ") end
  if title.text ~= head then title.text = head end
end
