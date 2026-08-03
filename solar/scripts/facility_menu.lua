-- FACILITIES MENU — walk the astronaut up to a base building and an
-- "⏎ Enter … [E]" prompt appears; press E (or click it) to open that facility's
-- panel. Press E or Esc (or the ✕ button) to leave.
--
--   Command Centre    → the company: balance, standing, the contract on the
--                       books — and the OPS BOARD, where contracts are signed.
--   Vehicle Assembly  → jump to the ship builder.
--   Tracking Station  → lists every launched vessel still holding a comms link
--                       (the `comms.ships` registry vessel_controller's power_tick
--                       writes — name / altitude / battery).
--   Power Plant       → base reactor readout.
--
-- Proximity is measured in WORLD space, via `world_of` below. A facility is
-- normally parented to its planet (so its own .x/.y/.z are body-relative and
-- ride the orbit) — but one that failed to find its planet node is top-level
-- and its coordinates are already world. Walking the parent chain is the one
-- reading that is right either way; comparing a raw .x against a body-relative
-- astronaut is off by the planet's orbital position (thousands of units) and
-- silently never matches.

-- `radius` is the fallback reach, measured from a building's ORIGIN, so it has
-- to cover the building itself now that you can stand INSIDE one: a prompt that
-- only fires outside the front door would go quiet exactly when you walked in.
-- Buildings differ in size, so most carry their own `near` below.
defaults = { radius = 13.0 }

local FAC = {
  -- near = prompt reach in metres. Command centre is 8.4 × 12.6 on the ground,
  -- the assembly gantry 7 × 7, the depot 5 × 5; the plant and the dish are
  -- machines you stand beside.
  FacCommand  = { label = "Command Centre",   kind = "command",  near = 16.0 },
  FacHangar   = { label = "Vehicle Assembly", kind = "hangar",   near = 14.0 },
  FacTracking = { label = "Tracking Station", kind = "tracking", near = 10.0 },
  FacPower    = { label = "Power Plant",      kind = "power",    near = 10.0 },
  FacDepot    = { label = "Commerce Depot",   kind = "depot",    near = 12.0 },
}

-- A node's WORLD position: its own coordinates plus every ancestor's. The chain
-- here (system group → planet → facility) carries no rotation and no scale, so
-- summing translations IS the world transform.
local function world_of(n)
  local x, y, z = n.x, n.y, n.z
  local p, guard = n.parent, 0
  while p and guard < 8 do
    x, y, z = x + p.x, y + p.y, z + p.z
    p, guard = p.parent, guard + 1
  end
  return x, y, z
end

-- Enter/leave: the E key OR the mapped "Interact" action (input.ron binds it to
-- E and the pad's west face), so the prompt answers a controller too. An action
-- the map doesn't carry reads false rather than erroring, which keeps the raw
-- key working in a project with no map at all.
local function interact_pressed()
  if input.pressed("e") then return true end
  return input.justPressed ~= nil and input.justPressed("Interact") == true
end

-- One console line, only when it CHANGES: "why is there no prompt?" is a
-- question the game should answer by itself — how many buildings exist, which
-- is nearest, how far. Silence means nothing changed, not that nothing ran.
local last_diag = ""
local function diag(msg)
  if msg ~= last_diag then
    last_diag = msg
    log("facilities: " .. msg)
  end
end
local ORDER = { "FacCommand", "FacHangar", "FacTracking", "FacPower", "FacDepot" }

-- How far from the depot a landed craft can be and still be unloaded by the
-- crane. Generous — you land where you can, not where a trigger volume is.
local UNLOAD_RANGE = 140.0

local prompt_n, prompt_el
local panel_el
local title_n, body_n, action_n, action_el
local open_fac = nil   -- name of the facility whose panel is open
local near_fac = nil   -- name of the nearest facility in range
-- The depot's last receipt ("sold for $412"), shown under the stock list for a
-- few seconds so the panel confirms what just happened without a popup.
local depot_msg, depot_msg_t = "", -100

local function grab()
  prompt_n = find("Facility Prompt")
  prompt_el = prompt_n and prompt_n:getcomponent("UiElement")
  local panel = find("Facility Panel")
  panel_el = panel and panel:getcomponent("UiElement")
  title_n = find("Facility Title")
  body_n = find("Facility Body")
  action_n = find("Facility Action")
  action_el = action_n and action_n:getcomponent("UiElement")
end

function start(node)
  grab()
  if prompt_el then prompt_el.visible = false end
  if panel_el then panel_el.visible = false end
end

local function fmt_alt(m)
  m = m or 0
  if math.abs(m) >= 1000 then return string.format("%.1f km", m / 1000) end
  return string.format("%.0f m", m)
end

-- The Tracking Station body: read the shared comms.ships registry live.
local function tracking_body()
  local ships = save.get("comms.ships")
  local list = {}
  if type(ships) == "table" then
    for _, s in pairs(ships) do if type(s) == "table" then list[#list + 1] = s end end
  end
  if #list == 0 then
    return "No vessels are transmitting.\n\nLaunch a craft carrying a comms dish\nand keep it powered — it will appear\nhere with live telemetry."
  end
  -- Stable order (UX: rows never jump around between refreshes).
  table.sort(list, function(a, b) return (a.name or "") < (b.name or "") end)
  local lines = { string.format("%d vessel(s) transmitting", #list), "" }
  for _, s in ipairs(list) do
    local pct = 0
    if (s.cap or 0) > 0 then pct = math.floor((s.charge or 0) / s.cap * 100 + 0.5) end
    lines[#lines + 1] = string.format("• %s  —  %s  —  bat %d%%", (s.name or "Vessel"), fmt_alt(s.alt), pct)
  end
  return table.concat(lines, "\n")
end

-- Is a facility panel up? The tool belt asks, so holding E at the depot digs a
-- hole in the floor instead of… well, exactly that.
function isOpen()
  return open_fac ~= nil
end

-- ── the depot ───────────────────────────────────────────────────────────────

-- Every vessel hold within crane reach of the depot that has something in it.
function landedHolds()
  local inv = findScript("inventory")
  local dep = find("FacDepot")
  if not (inv and dep) then return {} end
  -- Both in WORLD space (the depot is planet-parented, a landed craft usually
  -- isn't) — the same rule the proximity check below follows.
  local out = {}
  local dx0, dy0, dz0 = world_of(dep)
  for _, v in ipairs(findScripts("vessel_controller")) do
    local n = v.node
    if n and n.valid and v.holdId then
      local vx, vy, vz = world_of(n)
      local dx, dy, dz = vx - dx0, vy - dy0, vz - dz0
      if math.sqrt(dx * dx + dy * dy + dz * dz) < UNLOAD_RANGE
        and not inv.isEmpty(v.holdId) then
        out[#out + 1] = { id = v.holdId, name = v.craftName or "Vessel",
                          kg = inv.mass(v.holdId) }
      end
    end
  end
  return out
end

-- Unload: holds first, then the suit pack. Returns a sentence for the panel.
-- Global so the smoke harness (and any future crane button) can drive it.
function depotUnload()
  local inv = findScript("inventory")
  if not inv then return "no inventory system" end
  local moved, from = 0, 0
  for _, h in ipairs(landedHolds()) do
    local n = inv.transferAll(h.id, "base")
    if n > 0 then
      moved, from = moved + n, from + 1
      -- An emptied craft flies like an empty craft again: push the manifest's
      -- mass back into its bays.
      for _, v in ipairs(findScripts("vessel_controller")) do
        if v.holdId == h.id and v.holdApplyMass then v.holdApplyMass(v.node) end
      end
    end
  end
  local pack = inv.transferAll("astro", "base")
  moved = moved + pack
  if moved == 0 then return "nothing to unload" end
  return string.format("%d unit(s) into the warehouse%s", moved,
    from > 0 and string.format(" (%d craft)", from) or "")
end

-- Sell: the warehouse, at the registry's prices, nudged by standing. A first
-- sale of a material is a DISCOVERY — the moment the science half of the game
-- starts paying, and the one time a single crate is worth real money.
function depotSell()
  local inv, co, mats = findScript("inventory"), findScript("company"), findScript("materials")
  local rs = findScript("research")
  if not (inv and co and mats) then return "no depot systems" end
  local items = inv.items("base")
  if #items == 0 then return "the warehouse is empty" end
  local mult = 1.0 + 0.04 * co.rep()
  local total, bonus, firsts = 0, 0, {}
  local sold = save.get("depot.sold") or {}
  for _, it in ipairs(items) do
    total = total + it.value * mult
    if not sold[it.mat] then
      sold[it.mat] = true
      firsts[#firsts + 1] = mats.name(it.mat)
      -- The discovery premium: tier-scaled, paid once, ever.
      bonus = bonus + 150 * (1 + mats.tier(it.mat))
    end
    inv.remove("base", it.mat, it.n)
  end
  save.set("depot.sold", sold)
  total = math.floor(total)
  co.earn(total, "materials sold")
  if bonus > 0 then
    co.earn(bonus, "discovery premium")
    co.addRep(1, "first sale of " .. table.concat(firsts, ", "))
    if rs and rs.noteDiscovery then rs.noteDiscovery() end
  end
  return string.format("sold for %s%s", co.money(total),
    bonus > 0 and string.format("  ·  NEW DISCOVERY: %s (+%s)",
      table.concat(firsts, ", "), co.money(bonus)) or "")
end

-- The depot body: what's in the warehouse, what it's worth, and what's still
-- sitting in a hold out on the pad.
local function depot_body()
  local inv, co, mats = findScript("inventory"), findScript("company"), findScript("materials")
  if not (inv and co and mats) then return "Depot systems offline." end
  local lines = {}
  local holds = landedHolds()
  if #holds > 0 then
    lines[#lines + 1] = "ON THE PAD"
    for _, h in ipairs(holds) do
      lines[#lines + 1] = string.format("  %-18s %6.1f kg", h.name, h.kg)
    end
    lines[#lines + 1] = ""
  end
  local pack = inv.mass("astro")
  if pack > 0.05 then
    lines[#lines + 1] = string.format("Suit pack: %s", inv.line("astro"))
    lines[#lines + 1] = ""
  end
  local items = inv.items("base")
  lines[#lines + 1] = string.format("WAREHOUSE — %.1f kg", inv.mass("base"))
  if #items == 0 then
    lines[#lines + 1] = "  (empty)"
    lines[#lines + 1] = ""
    lines[#lines + 1] = "Mine rock with the laser, cut flora with"
    lines[#lines + 1] = "the harvester, fly it home in a Cargo Bay."
  else
    for _, it in ipairs(items) do
      lines[#lines + 1] = string.format("  %4d × %-16s %8s", it.n, mats.name(it.mat),
        co.money(it.value))
    end
    local mult = 1.0 + 0.04 * co.rep()
    lines[#lines + 1] = ""
    lines[#lines + 1] = string.format("Sale value: %s   (standing %+d%%)",
      co.money(inv.totalValue("base") * mult), math.floor(co.rep() * 4))
  end
  if depot_msg ~= "" and time - depot_msg_t < 8.0 then
    lines[#lines + 1] = ""
    lines[#lines + 1] = "» " .. depot_msg
  end
  return table.concat(lines, "\n")
end

local function refresh()
  if not open_fac then return end
  local f = FAC[open_fac]
  if title_n then title_n.text = f.label end
  local body, act = "", nil
  if f.kind == "tracking" then
    body = tracking_body()
  elseif f.kind == "hangar" then
    body = "Assemble and launch a new vessel\nfrom your saved parts."
    act = "Open Builder  ▸"
  elseif f.kind == "command" then
    local astro = find("Astronaut")
    local where = astro and space.dominant(astro.x, astro.y, astro.z) or "?"
    local ships = save.get("comms.ships")
    local n = 0
    if type(ships) == "table" then for _ in pairs(ships) do n = n + 1 end end
    -- The company's state, in the building the company is run from.
    local co = findScript("company")
    local mi = findScript("missions")
    local lines = { string.format("Base world: %s", where or "?"),
                    string.format("Vessels tracked: %d", n), "" }
    if co and co.money then
      lines[#lines + 1] = string.format("Balance:     %s", co.money(co.balance()))
      lines[#lines + 1] = string.format("Reputation:  %+d", co.rep())
    end
    if mi then
      lines[#lines + 1] = ""
      lines[#lines + 1] = mi.active
        and ("Contract:    " .. (mi.activeLine() or ""))
        or  ("Contract:    none — sign one at the Ops Board")
      if mi.active then
        for _, l in ipairs(mi.goalLines()) do lines[#lines + 1] = "   " .. l end
      end
    end
    if co and co.ledgerLines then
      local led = co.ledgerLines(5)
      if #led > 0 then
        lines[#lines + 1] = ""
        lines[#lines + 1] = "Recent:"
        for _, l in ipairs(led) do lines[#lines + 1] = "  " .. l end
      end
    end
    body = table.concat(lines, "\n")
    act = "Ops Board  ▸"
  elseif f.kind == "power" then
    body = "Reactor: ONLINE\n\nThe plant powers the base grid and\ntops off docked craft."
  elseif f.kind == "depot" then
    body = depot_body()
    -- One button, two steps, in the order you actually do them: get the cargo
    -- off the craft, then sell what's in the shed.
    local inv = findScript("inventory")
    if inv then
      if #landedHolds() > 0 or inv.mass("astro") > 0.05 then
        act = "Unload  ▸"
      elseif not inv.isEmpty("base") then
        act = "Sell Everything  ▸"
      end
    end
  end
  if body_n then body_n.text = body end
  if action_n and act then action_n.text = act end
  if action_el then action_el.visible = act ~= nil end
end

-- Called by the panel's ✕ button and by pressing E/Esc while open.
function close()
  open_fac = nil
  if panel_el then panel_el.visible = false end
end

function openMenu(name)
  if not FAC[name] then return end
  open_fac = name
  if prompt_el then prompt_el.visible = false end
  if panel_el then panel_el.visible = true end
  refresh()
end

-- The action button routes by the open facility's kind.
function action()
  if not open_fac then return end
  local kind = FAC[open_fac].kind
  if kind == "hangar" then
    scene.load("builder")
  elseif kind == "command" then
    -- The Command Centre opens the OPS BOARD. Leaving for the main menu moved
    -- to the HUD's ☰ button, which is where every other "quit" already lives —
    -- a facility panel shouldn't be the only route out of the world.
    local ob = findScript("ops_board")
    close()
    if ob and ob.openBoard then ob.openBoard() end
  elseif kind == "depot" then
    local inv = findScript("inventory")
    if inv and (#landedHolds() > 0 or inv.mass("astro") > 0.05) then
      depot_msg = depotUnload()
    else
      depot_msg = depotSell()
    end
    depot_msg_t = time
    refresh()
  else
    close()
  end
end

function update(node, dt)
  if not panel_el then grab() end
  if not panel_el then
    diag("no \"Facility Panel\" node in this scene — nothing can open")
    return
  end

  local astro = find("Astronaut")
  -- Only the on-foot astronaut interacts. While piloting / loading the model is
  -- hidden — no prompts, and any open panel closes.
  if not astro then
    diag("no Astronaut in the scene")
  elseif astro.visible == false then
    diag("the astronaut is hidden (you're aboard a vessel) — prompts are off")
  end
  if not astro or astro.visible == false then
    near_fac = nil
    if prompt_el then prompt_el.visible = false end
    if open_fac then close() end
    return
  end

  -- Nearest facility within ITS reach, in world space.
  near_fac = nil
  local ax, ay, az = world_of(astro)
  local standing, nearest_d, nearest_n, best_in = 0, nil, nil, nil
  for _, name in ipairs(ORDER) do
    local f = find(name)
    if f then
      standing = standing + 1
      local fx, fy, fz = world_of(f)
      local dx, dy, dz = fx - ax, fy - ay, fz - az
      local dist = math.sqrt(dx * dx + dy * dy + dz * dz)
      if not nearest_d or dist < nearest_d then nearest_d, nearest_n = dist, name end
      if dist < (FAC[name].near or params.radius) and (not best_in or dist < best_in) then
        best_in, near_fac = dist, name
      end
    end
  end
  -- Report the SHAPE of the situation, not every metre walked: the building
  -- count and which one you're at. Walking around the base doesn't spam it.
  if standing == 0 then
    diag("none standing yet — base_facilities hasn't sited them")
  elseif near_fac then
    diag(string.format("%d standing · at %s", standing, FAC[near_fac].label))
  else
    diag(string.format("%d standing · nearest %s, %.0f m away", standing,
      nearest_n and FAC[nearest_n].label or "?", nearest_d or 0))
  end

  if open_fac then
    if prompt_el then prompt_el.visible = false end
    refresh() -- keep telemetry live while the panel is up
    if interact_pressed() or input.pressed("escape") then close() end
    return
  end

  local ob = findScript("ops_board")
  if ob and ob.isOpen and ob.isOpen() then
    if prompt_el then prompt_el.visible = false end
    if interact_pressed() then ob.closeBoard() end
    return
  end
  if near_fac then
    if prompt_n then prompt_n.text = "⏎  Enter " .. FAC[near_fac].label .. "   [E]" end
    if prompt_el then prompt_el.visible = true end
    if interact_pressed() then openMenu(near_fac) end
  elseif prompt_el then
    prompt_el.visible = false
  end
end
