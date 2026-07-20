-- NEW GALAXY panel (attached to the "NG Panel" node): shown when the player
-- clicks an EMPTY save slot. Every system-generator knob is exposed on a
-- draggable slider inside the "NG Params" SCROLL VIEW (wheel to reach them
-- all), plus a galaxy seed typed on the keyboard (digits; backspace erases;
-- blank = random). CREATE stores the whole parameter set in the fresh slot
-- and enters the game — game_manager reads it back and hands it to the
-- generator, so the same save always regenerates the same galaxy. The
-- panel's buttons carry menu_ng_button, which routes clicks here (press).

open = false -- published: menu_slot opens the panel, buttons check it

-- One row per generator knob: the scroll-view node names, the slot key it
-- saves under, the generator opt it overrides, and how its value reads.
local PARAMS = {
  { node = "NG Planets",  save = "g_planets",    opt = "planets",    fmt = "int" },
  { node = "NG Moons",    save = "g_moonchance", opt = "moonChance", fmt = "pct" },
  { node = "NG Atmo",     save = "g_atmo",       opt = "atmoChance", fmt = "pct" },
  { node = "NG Caves",    save = "g_caves",      opt = "caveScale",  fmt = "x" },
  { node = "NG MinOrbit", save = "g_minorbit",   opt = "minOrbit",   fmt = "int" },
  { node = "NG Spacing",  save = "g_spacing",    opt = "spacing",    fmt = "x" },
  { node = "NG RadMin",   save = "g_radmin",     opt = "radiusMin",  fmt = "int" },
  { node = "NG RadMax",   save = "g_radmax",     opt = "radiusMax",  fmt = "int" },
  { node = "NG GravMin",  save = "g_gravmin",    opt = "gravityMin", fmt = "g" },
  { node = "NG GravMax",  save = "g_gravmax",    opt = "gravityMax", fmt = "g" },
  { node = "NG Star",     save = "g_star",       opt = "starScale",  fmt = "x" },
}

local slot_n = 1
local seed_str = ""

local function ui_of(name)
  local n = find(name)
  return n, n and n:getcomponent("UiElement")
end

local function slider_of(p)
  local n = find(p.node .. " Track")
  local s = n and n:getcomponent("UiSlider")
  return s and s.value
end

local function value_of(p)
  local v = slider_of(p)
  if not v then return nil end
  if p.fmt == "int" then v = math.floor(v + 0.5) end
  return v
end

local function seed_label()
  local n = find("NG Seed")
  if not n then return end
  if #seed_str > 0 then
    n.text = "SEED   " .. seed_str
  else
    n.text = "SEED   random   (type digits)"
  end
end

-- The two panels sit centered in the same layer: exactly ONE is ever visible.
-- Hiding a panel's root prunes its whole subtree from layout + hit-testing,
-- so the swap is a clean page turn, never an overlay.
local function show(panel_open)
  local _, ng = ui_of("NG Panel")
  if ng then ng.visible = panel_open end
  local _, main = ui_of("Menu Panel")
  if main then main.visible = not panel_open end
end

function openFor(n)
  slot_n = n
  seed_str = ""
  open = true
  local lbl = find("NG Slot Label")
  if lbl then lbl.text = string.format("— creating save in slot %d —", n) end
  seed_label()
  local _, sc = ui_of("NG Params")
  if sc then sc.scrollY = 0 end
  show(true)
end

local function close()
  open = false
  show(false)
end

-- Button router (menu_ng_button forwards its role here).
function press(role)
  if role == "cancel" then
    close()
  elseif role == "seed" then
    seed_str = "" -- click the seed row to clear back to random
    seed_label()
  elseif role == "create" then
    save.slot("slot" .. slot_n)
    local v = {}
    for _, p in ipairs(PARAMS) do v[p.save] = value_of(p) end
    -- A min above its max would corrupt the generator's ranges — the max wins
    -- up to the min (silently: the sliders still show what was dragged).
    if v.g_radmax and v.g_radmin and v.g_radmax < v.g_radmin then v.g_radmax = v.g_radmin end
    if v.g_gravmax and v.g_gravmin and v.g_gravmax < v.g_gravmin then v.g_gravmax = v.g_gravmin end
    for _, p in ipairs(PARAMS) do
      if v[p.save] then save.set(p.save, v[p.save]) end
    end
    if #seed_str > 0 then save.set("g_seed", tonumber(seed_str)) end
    scene.load("system")
  end
end

function update(node, dt)
  if not open then return end
  -- Seed entry: the panel is the only text input on screen, so bare digit
  -- keys type into it. 9 digits max keeps seeds in the generator's range.
  for d = 0, 9 do
    if input.pressed(tostring(d)) and #seed_str < 9 then
      seed_str = seed_str .. d
      seed_label()
    end
  end
  if input.pressed("backspace") and #seed_str > 0 then
    seed_str = seed_str:sub(1, -2)
    seed_label()
  end
  -- Live value readouts beside each slider.
  for _, p in ipairs(PARAMS) do
    local v = slider_of(p)
    local out = find(p.node .. " Val")
    if v and out then
      if p.fmt == "int" then
        out.text = string.format("%d", math.floor(v + 0.5))
      elseif p.fmt == "pct" then
        out.text = string.format("%d%%", math.floor(v * 100 + 0.5))
      elseif p.fmt == "g" then
        out.text = string.format("%.1f", v)
      else -- "x"
        out.text = string.format("×%.2f", v)
      end
    end
  end
end
