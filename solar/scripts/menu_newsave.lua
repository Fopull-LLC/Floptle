-- NEW GALAXY panel (attached to the "NG Panel" node): shown when the player
-- clicks an EMPTY save slot. They pick the generation parameters on draggable
-- sliders (planet count, moon chance, atmosphere chance, cave scale), type a
-- galaxy seed on the keyboard (digits; backspace erases; blank = random), and
-- CREATE stores everything in the fresh slot and enters the game —
-- game_manager reads the parameters back and hands them to the generator, so
-- the same save always regenerates the same galaxy. The panel's buttons carry
-- menu_ng_button, which routes their clicks here (press(role)).

open = false -- published: menu_slot opens the panel, buttons check it

local panel
local slot_n = 1
local seed_str = ""

local function ui_of(name)
  local n = find(name)
  return n, n and n:getcomponent("UiElement")
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

function openFor(n)
  slot_n = n
  seed_str = ""
  open = true
  local lbl = find("NG Slot Label")
  if lbl then lbl.text = string.format("— creating save in slot %d —", n) end
  seed_label()
  local _, el = ui_of("NG Panel")
  if el then el.visible = true end
end

local function close()
  open = false
  local _, el = ui_of("NG Panel")
  if el then el.visible = false end
end

local function slider(name)
  local n = find(name)
  local s = n and n:getcomponent("UiSlider")
  return s and s.value
end

-- Button router (menu_ng_button forwards its role here).
function press(role)
  if role == "cancel" then
    close()
  elseif role == "seed" then
    seed_str = "" -- click the seed row to clear back to random
    seed_label()
  elseif role == "create" then
    local name = "slot" .. slot_n
    save.slot(name)
    save.set("g_planets", math.floor((slider("NG Planets Track") or 3) + 0.5))
    save.set("g_moonchance", slider("NG Moons Track") or 0.6)
    save.set("g_atmo", slider("NG Atmo Track") or 0.8)
    save.set("g_caves", slider("NG Caves Track") or 1.0)
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
  local p = slider("NG Planets Track")
  local pv = find("NG Planets Val")
  if p and pv then pv.text = string.format("%d", math.floor(p + 0.5)) end
  local m = slider("NG Moons Track")
  local mv = find("NG Moons Val")
  if m and mv then mv.text = string.format("%d%%", math.floor(m * 100 + 0.5)) end
  local a = slider("NG Atmo Track")
  local av = find("NG Atmo Val")
  if a and av then av.text = string.format("%d%%", math.floor(a * 100 + 0.5)) end
  local c = slider("NG Caves Track")
  local cv = find("NG Caves Val")
  if c and cv then cv.text = string.format("×%.2f", c) end
end
