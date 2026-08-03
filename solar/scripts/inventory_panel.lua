-- INVENTORY PANEL — what's in the suit pack, and where it can go from here.
--
-- Opened with I on foot. It lists the pack's contents (one `inv_row` each) and,
-- when you're standing next to something that can hold cargo, offers to move it:
--
--   next to a landed craft with a Cargo Bay  → its hold
--   next to the Commerce Depot               → the warehouse
--
-- Click a row to move that one stack; the button moves everything that fits.
-- Whatever doesn't fit stays in the pack — a transfer never destroys anything,
-- and a full hold says so rather than silently eating the rest.
--
-- Moving cargo into a craft re-applies its mass (vessel_controller.holdApplyMass),
-- so the ship you just loaded actually flies like a loaded ship.

defaults = {
  reach = 11.0,   -- how close a craft has to be to load it from the ground
}

-- Published.
open = false
targetId = nil
targetLabel = nil

local el, title, hint, action, action_el

local function grab()
  if not title then title = find("Inventory Title") end
  if not hint then hint = find("Inventory Hint") end
  if not action then
    action = find("Inventory Action")
    action_el = action and action:getcomponent("UiElement")
  end
end

local function on_foot()
  local astro = find("Astronaut")
  if not astro or astro.visible == false then return nil end
  for _, v in ipairs(findScripts("vessel_controller")) do
    if v.piloting then return nil end
  end
  local sc = findScript("ship_controller")
  if sc and sc.piloting then return nil end
  return astro
end

-- What's in reach that can take cargo. A craft wins over the warehouse if both
-- are close — you walked to the ship for a reason.
local function find_target(astro)
  local inv = findScript("inventory")
  if not inv then return nil, nil end
  local best, bestd, label = nil, params.reach, nil
  for _, v in ipairs(findScripts("vessel_controller")) do
    local n = v.node
    if n and n.valid and v.holdId then
      local d = math.sqrt((n.x - astro.x) ^ 2 + (n.y - astro.y) ^ 2 + (n.z - astro.z) ^ 2)
      if d < bestd then
        best, bestd, label = v.holdId, d, (v.craftName or "craft") .. " hold"
      end
    end
  end
  if best then return best, label end
  -- The depot: planet-parented, so its own coordinates are body-relative and
  -- only become world space through its parent chain (see facility_menu).
  local dep = find("FacDepot")
  if dep then
    local dx0, dy0, dz0 = dep.x, dep.y, dep.z
    local p, guard = dep.parent, 0
    while p and guard < 8 do
      dx0, dy0, dz0 = dx0 + p.x, dy0 + p.y, dz0 + p.z
      p, guard = p.parent, guard + 1
    end
    local dx, dy, dz = astro.x - dx0, astro.y - dy0, astro.z - dz0
    if math.sqrt(dx * dx + dy * dy + dz * dz) < 14.0 then
      return "base", "warehouse"
    end
  end
  return nil, nil
end

-- The vessel script owning a hold id (so a transfer can re-apply its mass).
local function vessel_of(hold)
  for _, v in ipairs(findScripts("vessel_controller")) do
    if v.holdId == hold then return v end
  end
  return nil
end

local msg, msg_t = "", -100

-- Move one stack. Called by a row's click.
function moveStack(mat)
  local inv = findScript("inventory")
  if not (inv and targetId) then return end
  local have = inv.count("astro", mat)
  local moved = inv.transfer("astro", targetId, mat, have)
  local mats = findScript("materials")
  local nm = (mats and mats.name(mat)) or mat
  if moved == 0 then
    msg = string.format("%s won't fit in the %s", nm, targetLabel or "hold")
  elseif moved < have then
    msg = string.format("%d of %d %s moved — %s full", moved, have, nm, targetLabel or "hold")
  else
    msg = string.format("%d × %s → %s", moved, nm, targetLabel or "hold")
  end
  msg_t = time
  local v = vessel_of(targetId)
  if v and v.holdApplyMass then v.holdApplyMass(v.node) end
end

function moveAll()
  local inv = findScript("inventory")
  if not (inv and targetId) then return end
  local before = inv.mass("astro")
  local n = inv.transferAll("astro", targetId)
  msg_t = time
  if n == 0 then
    msg = (before > 0.05) and ("nothing fits in the " .. (targetLabel or "hold"))
      or "the pack is empty"
  else
    msg = string.format("%d unit(s) → %s%s", n, targetLabel or "hold",
      inv.mass("astro") > 0.05 and "  (the rest didn't fit)" or "")
  end
  local v = vessel_of(targetId)
  if v and v.holdApplyMass then v.holdApplyMass(v.node) end
end

function start(node)
  el = node:getcomponent("UiElement")
  if el then el.visible = false end
  open = false
end

function update(node, dt)
  if not el then el = node:getcomponent("UiElement") end
  if not el then return end
  grab()
  local astro = on_foot()
  if not astro then
    open = false
    el.visible = false
    return
  end
  local gm = findScript("game_manager")
  if gm and gm.loading then el.visible = false; return end
  if input.pressed("i") then open = not open end
  if input.pressed("escape") and open then open = false end
  el.visible = open
  if not open then
    targetId, targetLabel = nil, nil
    return
  end

  targetId, targetLabel = find_target(astro)
  local inv = findScript("inventory")
  local cl = findScript("climate")
  if title and inv then
    local head = string.format("INVENTORY   ·   suit pack %s", inv.line("astro"))
    local where = cl and cl.surveyLine(astro.x, astro.y, astro.z)
    if where then head = head .. "\n" .. where end
    if inv.totalValue("astro") > 0 then
      local co = findScript("company")
      head = head .. string.format("   ·   worth %s at the depot",
        (co and co.money(inv.totalValue("astro"))) or ("$" .. inv.totalValue("astro")))
    end
    if title.text ~= head then title.text = head end
  end
  if action_el then
    action_el.visible = targetId ~= nil
    if action and targetId then
      local t = string.format("  Transfer all  →  %s  ▸", targetLabel or "hold")
      if action.text ~= t then action.text = t end
    end
  end
  if hint then
    local t
    if time - msg_t < 4.0 and msg ~= "" then
      t = "  " .. msg
    elseif targetId then
      t = "  click a row to move that stack   ·   I or Esc to close"
    elseif inv and inv.isEmpty("astro") then
      t = "  empty — mine rock with [1], harvest flora with [2]"
    else
      t = "  stand by a craft with a Cargo Bay, or at the depot, to unload"
    end
    if hint.text ~= t then hint.text = t end
  end
end
