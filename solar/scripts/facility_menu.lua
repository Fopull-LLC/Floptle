-- FACILITIES MENU — walk the astronaut up to a base building and an
-- "⏎ Enter … [E]" prompt appears; press E (or click it) to open that facility's
-- panel. Press E or Esc (or the ✕ button) to leave.
--
--   Command Centre    → base overview + return to the main menu.
--   Vehicle Assembly  → jump to the ship builder.
--   Tracking Station  → lists every launched vessel still holding a comms link
--                       (the `comms.ships` registry vessel_controller's power_tick
--                       writes — name / altitude / battery).
--   Power Plant       → base reactor readout.
--
-- Proximity is measured in the BODY-RELATIVE frame so it survives the planet's
-- orbit: the facilities are planet-parented (their .x/.y/.z ARE the body-relative
-- offset already) and the astronaut is top-level (world), so subtracting the
-- dominant body from the astronaut puts both in the same frame.

defaults = { radius = 9.0 }

local FAC = {
  FacCommand  = { label = "Command Centre",   kind = "command" },
  FacHangar   = { label = "Vehicle Assembly", kind = "hangar" },
  FacTracking = { label = "Tracking Station", kind = "tracking" },
  FacPower    = { label = "Power Plant",      kind = "power" },
}
local ORDER = { "FacCommand", "FacHangar", "FacTracking", "FacPower" }

local prompt_n, prompt_el
local panel_el
local title_n, body_n, action_n, action_el
local open_fac = nil   -- name of the facility whose panel is open
local near_fac = nil   -- name of the nearest facility in range

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
    body = string.format("Base world: %s\nVessels tracked: %d\n\nAll base systems nominal.", where or "?", n)
    act = "Return to Menu"
  elseif f.kind == "power" then
    body = "Reactor: ONLINE\n\nThe plant powers the base grid and\ntops off docked craft."
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
    local gm = findScript("game_manager")
    if gm and gm.exitToMenu then gm.exitToMenu() else scene.load("menu") end
  else
    close()
  end
end

function update(node, dt)
  if not panel_el then grab() end
  if not panel_el then return end

  local astro = find("Astronaut")
  -- Only the on-foot astronaut interacts. While piloting / loading the model is
  -- hidden — no prompts, and any open panel closes.
  if not astro or astro.visible == false then
    near_fac = nil
    if prompt_el then prompt_el.visible = false end
    if open_fac then close() end
    return
  end

  -- Nearest facility within reach, in the body-relative frame.
  near_fac = nil
  local d = space.dominant(astro.x, astro.y, astro.z)
  local b = d and space.body(d)
  if b then
    local arx, ary, arz = astro.x - b.x, astro.y - b.y, astro.z - b.z
    local best = params.radius
    for _, name in ipairs(ORDER) do
      local f = find(name)
      if f then
        local dx, dy, dz = f.x - arx, f.y - ary, f.z - arz
        local dist = math.sqrt(dx * dx + dy * dy + dz * dz)
        if dist < best then best = dist; near_fac = name end
      end
    end
  end

  if open_fac then
    if prompt_el then prompt_el.visible = false end
    refresh() -- keep telemetry live while the panel is up
    if input.pressed("e") or input.pressed("escape") then close() end
    return
  end

  if near_fac then
    if prompt_n then prompt_n.text = "⏎  Enter " .. FAC[near_fac].label .. "   [E]" end
    if prompt_el then prompt_el.visible = true end
    if input.pressed("e") then openMenu(near_fac) end
  elseif prompt_el then
    prompt_el.visible = false
  end
end
