-- SMOKE HARNESS for facility_menu.lua + facility_button.lua: stubs the engine,
-- stands the astronaut next to the Tracking Station, and drives open → read
-- registry → action/close. Catches Lua errors and the interaction logic.
--
--   luajit solar/tests/smoke_facilities.lua

_G.time = 0.0

-- Body-relative frame: planet offset from origin, astronaut top-level (world),
-- facilities planet-parented (their .x/.y/.z ARE body-relative), exactly like the
-- real scenes.
local planet = { name = "Athosil", x = 300, y = -120, z = 80, radius = 200, soi = 5000 }

local function el() return { visible = true, opacity = 1.0 } end
local function ui_node(name)
  return { name = name, text = "", __el = el(),
    getcomponent = function(self, k) if k == "UiElement" then return self.__el end end }
end

local nodes = {}
local function add(n) nodes[n.name] = n; return n end

-- The astronaut, in WORLD coords, standing 4 m from where FacTracking sits.
local astro = add({ name = "Astronaut", visible = true,
  x = planet.x + 0, y = planet.y + planet.radius, z = planet.z + 0 })

-- FacTracking parented to the planet → its coords ARE body-relative. Put it 4 m
-- north of the astronaut's body-relative position (0, R, 0).
add({ name = "FacTracking", x = 0, y = planet.radius, z = 4 })
add({ name = "FacCommand",  x = 40, y = planet.radius, z = 0 })
add({ name = "FacHangar",   x = 0,  y = planet.radius, z = 40 })
add({ name = "FacPower",    x = -40, y = planet.radius, z = 0 })

-- UI nodes the script drives.
for _, nm in ipairs({ "Facility Prompt", "Facility Panel", "Facility Title", "Facility Body", "Facility Action", "Facility Exit" }) do
  add(ui_node(nm))
end

_G.find = function(name) return nodes[name] end
_G.space = {
  dominant = function() return planet.name end,
  body = function(n) if n == planet.name then return planet end end,
}
local SAVE = {}
_G.save = { get = function(k) return SAVE[k] end, set = function(k, v) SAVE[k] = v end }

local KEYS = {}
_G.input = { pressed = function(k) return KEYS[k] == true end, key = function() return false end }
local LOADED = nil
_G.scene = { load = function(s) LOADED = s end }
_G.log = function() end

-- Two live vessels in the tracker registry (one full battery, one half).
SAVE["comms.ships"] = {
  v1 = { name = "Odyssey", x = 1, y = 2, z = 3, alt = 1250, charge = 200, cap = 200, t = 0 },
  v7 = { name = "Scout",   x = 4, y = 5, z = 6, alt = 84,   charge = 50,  cap = 100, t = 0 },
}

-- ── load both scripts under distinct script tables (findScript resolves them) ──
local menu_env = {}
local function load_into(path)
  local chunk = assert(loadfile(path))
  chunk()
  return {
    start = start, update = update, action = action, close = close,
    openMenu = openMenu, clicked = clicked, params = _G.params,
  }
end

-- facility_menu first (it defines start/update/openMenu/action/close as globals).
local menu = load_into("solar/scripts/facility_menu.lua")
menu.params = { radius = 9.0 }
_G.params = menu.params
_G.findScript = function(kind)
  if kind == "facility_menu" then return menu end
  if kind == "game_manager" then return { exitToMenu = function() LOADED = "menu" end } end
  return nil
end

menu.start({ name = "Facilities Menu" })
assert(nodes["Facility Prompt"].__el.visible == false, "prompt should start hidden")
assert(nodes["Facility Panel"].__el.visible == false, "panel should start hidden")

-- Tick once: astronaut is 4 m from FacTracking (< radius 9) → prompt shows.
menu.update({}, 1 / 60)
assert(nodes["Facility Prompt"].__el.visible == true, "prompt should show near a facility")
assert(nodes["Facility Prompt"].text:find("Tracking Station"), "prompt should name the nearest facility, got: " .. nodes["Facility Prompt"].text)

-- Press E → panel opens on the Tracking Station.
KEYS["e"] = true
menu.update({}, 1 / 60)
KEYS["e"] = false
assert(nodes["Facility Panel"].__el.visible == true, "panel should open on E")
assert(nodes["Facility Prompt"].__el.visible == false, "prompt hidden while panel open")
assert(nodes["Facility Title"].text == "Tracking Station", "title should be the facility")
local body = nodes["Facility Body"].text
assert(body:find("2 vessel"), "tracking body should count 2 vessels, got: " .. body)
assert(body:find("Odyssey") and body:find("Scout"), "tracking body should list both ships")
assert(body:find("bat 100%", 1, true) and body:find("bat 50%", 1, true), "tracking body should show battery %, got: " .. body)

-- Press E again → closes.
KEYS["e"] = true
menu.update({}, 1 / 60)
KEYS["e"] = false
assert(nodes["Facility Panel"].__el.visible == false, "panel should close on E")

-- Empty registry → friendly message, no crash.
SAVE["comms.ships"] = {}
menu.openMenu("FacTracking")
assert(nodes["Facility Body"].text:find("No vessels"), "empty registry should show the empty message")
menu.close()

-- Hangar action → jumps to the builder.
menu.openMenu("FacHangar")
assert(nodes["Facility Action"].__el.visible == true, "hangar should show an action button")
menu.action()
assert(LOADED == "builder", "hangar action should load the builder scene, got: " .. tostring(LOADED))

-- Walking away hides the prompt; boarding (invisible astronaut) closes the panel.
menu.openMenu("FacCommand")
astro.visible = false
menu.update({}, 1 / 60)
assert(nodes["Facility Panel"].__el.visible == false, "panel closes when the astronaut is hidden (piloting)")
astro.visible = true
astro.z = planet.z + 500 -- far from every facility
menu.update({}, 1 / 60)
assert(nodes["Facility Prompt"].__el.visible == false, "prompt hidden when far from all facilities")

print("smoke_facilities OK — proximity, open/close, tracking readout, hangar→builder, board/leave")
