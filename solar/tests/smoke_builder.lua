-- SMOKE HARNESS: runs the real builder.lua against a stubbed engine API and
-- drives the SYMMETRY feature end-to-end — pick, free-place, stack-snap,
-- ×4 radial ring, auto-mirrored stacking, ring scrap, undo. Any Lua error in
-- the builder fails loudly with a traceback; the assertions catch geometry
-- regressions (the ring math is easy to break silently).
--
--   luajit solar/tests/smoke_builder.lua
--
-- The stub camera projects orthographically (screen = world × 100 + 500), so
-- "moving the mouse onto an attach node" is just projecting the node's world
-- position — the same math find_snap uses to capture the ghost.

local T = 0.0
local TICK = 1 / 60

-- ── nodes ────────────────────────────────────────────────────────────────────
local nodes = {}
local next_id = 1
local function make_node(name, x, y, z)
  local n = { __id = next_id, id = next_id, name = name, valid = true,
              visible = true, x = x or 0, y = y or 0, z = z or 0,
              pitch = 0, roll = 0, yaw = 0, text = "", components = {} }
  n.getcomponent = function(self, kind) return self.components[kind] end
  next_id = next_id + 1
  nodes[#nodes + 1] = n
  return n
end

-- ── stub API ─────────────────────────────────────────────────────────────────
local API = {}
local save_store = {}

function API.vec3(x, y, z) return { x = x or 0, y = y or 0, z = z or 0 } end
function API.destroy(n) n.valid = false end
function API.find(name)
  for _, n in ipairs(nodes) do
    if n.valid and n.name == name then return n end
  end
  return nil
end
API.print = function() end
API.log = API.print

-- Spawns land SYNCHRONOUSLY (the builder tolerates async; sync keeps the
-- test linear).
function API.spawn(prefab, pos, cb)
  local n = make_node(prefab, pos.x, pos.y, pos.z)
  if cb then cb(n) end
end

API.KEYS = { down = {}, edge = {} }
local MOUSE = { x = 500, y = 500, lmb = false }
API.input = {
  key = function(k) return API.KEYS.down[k] or false end,
  pressed = function(k) return API.KEYS.edge[k] or false end,
  button = function(b) return b == 0 and MOUSE.lmb or false end,
  mouse = function() return MOUSE.x, MOUSE.y end,
  mouse_delta = function() return 0, 0 end,
  scroll = function() return 0 end,
  setMouseLocked = function() end,
}

-- Orthographic-ish camera: screen = world × 100 + 500 (Y flipped), and the
-- cursor ray drops straight down at the un-projected XZ.
API.camera = {
  worldToScreen = function(x, y, z)
    return x * 100 + 500 + z * 3, -y * 100 + 500 + z * 3, 1.0, true
  end,
  screenToRay = function(mx, my)
    return (mx - 500) / 100, 10.0, 0.0, 0.0, -1.0, 0.0
  end,
}

function API.raycast() return nil end
API.draw = {
  line = function() end, ring = function() end,
  sphere = function() end, box = function() end,
}
API.save = {
  get = function(k) return save_store[k] end,
  set = function(k, v) save_store[k] = v end,
  flush = function() end,
}
API.scene = { load = function() end }

-- ── load the builder ─────────────────────────────────────────────────────────
make_node("BuildStats", 0, 0, 0)
make_node("BuildHint", 0, 0, 0)

local env = setmetatable({}, { __index = function(_, k)
  if k == "time" then return T end
  return API[k] or _G[k]
end })
local chunk = assert(loadfile("solar/scripts/builder.lua"))
setfenv(chunk, env)
chunk()
env.params = {}
for k, v in pairs(env.defaults or {}) do env.params[k] = v end

local builder_node = make_node("Builder", 0, 0, 0)
env.start(builder_node)

local failures = {}
local function check(cond, what)
  if not cond then failures[#failures + 1] = "CHECK FAILED: " .. what end
end

local function step(n)
  for _ = 1, n or 1 do
    T = T + TICK
    local ok, err = xpcall(env.update, debug.traceback, builder_node, TICK)
    if not ok then failures[#failures + 1] = "update: " .. err end
    API.KEYS.edge = {}
  end
end

local function press(k) API.KEYS.edge[k] = true end
local function mouse_to_world(x, y, z)
  local sx, sy = API.camera.worldToScreen(x, y, z)
  MOUSE.x, MOUSE.y = sx, sy
end
local function click()
  MOUSE.lmb = true
  step(1)
  MOUSE.lmb = false
  step(14) -- let the click cooldown lapse
end

-- The builder's internal registry, read back through the saved blueprint.
local function saved_parts()
  env.doSave()
  local bp = save_store["shipyard.blueprint"]
  local out = {}
  for _, d in pairs(bp.parts) do out[#out + 1] = d end
  return out
end

-- ── scenario ─────────────────────────────────────────────────────────────────
step(2)

-- 1. A pod, placed free on the pad (the first part needs no attach node).
env.pick("pod")
step(12)
mouse_to_world(0, 0.5, 0)
click()
check(env.partCount == 1, "pod placed free (partCount=" .. tostring(env.partCount) .. ")")

-- The pod's world position (its center) — attach/side nodes hang off it.
local pod_x, pod_y, pod_z
for _, n in ipairs(nodes) do
  if n.valid and n.name == "PartPod" then pod_x, pod_y, pod_z = n.x, n.y, n.z end
end
check(pod_x ~= nil, "pod node exists")

-- 2. A tank stack-snapped UNDER the pod (bottom node).
env.pick("tankS")
step(12)
mouse_to_world(pod_x, pod_y - 0.4, pod_z)
click()
check(env.partCount == 2, "tank snapped under the pod (partCount=" .. tostring(env.partCount) .. ")")
local tank_x, tank_y, tank_z
for _, n in ipairs(nodes) do
  if n.valid and n.name == "PartTankS" then tank_x, tank_y, tank_z = n.x, n.y, n.z end
end
check(tank_y and tank_y < pod_y, "tank sits below the pod")

-- 3. SYMMETRY ×4: three X presses (1→2→3→4), then one radial decoupler on
--    the tank's flank → a ring of FOUR, evenly spaced, sharing a group.
press("x") step(1)
press("x") step(1)
press("x") step(1)
env.pick("radialDec")
step(12)
mouse_to_world(tank_x + 0.5, tank_y, tank_z) -- the +X flank node
step(1)                                      -- let the ghost snap this frame
click()
check(env.partCount == 6, "×4 symmetry placed a ring of four (partCount=" .. tostring(env.partCount) .. ")")
local ring = {}
for _, d in ipairs(saved_parts()) do
  if d.id == "radialDec" then ring[#ring + 1] = d end
end
check(#ring == 4, "blueprint has 4 radial decouplers (" .. #ring .. ")")
if #ring == 4 then
  local gid = ring[1].sym
  local r0 = nil
  local ok_group, ok_radius = true, true
  for _, d in ipairs(ring) do
    if d.sym ~= gid or gid == 0 then ok_group = false end
    -- Blueprint x/z are ship-center-relative; the ring is centered on the
    -- stack (x≈z≈0 axis), so all radii must match.
    local r = math.sqrt(d.x * d.x + d.z * d.z)
    r0 = r0 or r
    if math.abs(r - r0) > 0.01 then ok_radius = false end
  end
  check(ok_group, "ring members share one symmetry group")
  check(ok_radius, "ring members sit at one radius")
  -- Even spacing: angles 90° apart.
  local angs = {}
  for _, d in ipairs(ring) do angs[#angs + 1] = math.atan2(d.z, d.x) end
  table.sort(angs)
  for i = 2, #angs do
    local da = angs[i] - angs[i - 1]
    check(math.abs(da - math.pi / 2) < 0.02,
      string.format("ring spacing 90° (got %.1f°)", math.deg(da)))
  end
end

-- 4. AUTO-MIRROR: one engine on ONE decoupler's outer flank → four engines,
--    one per decoupler, in their own chained group.
env.pick("engineS")
step(12)
-- The +X decoupler's outward FACE node: dec center sits at tank flank +
-- its rolled half-thickness (0.5 + 0.125); the face is another 0.125 out.
mouse_to_world(tank_x + 0.75, tank_y, tank_z)
step(1)
click()
local engines = {}
for _, d in ipairs(saved_parts()) do
  if d.id == "engineS" then engines[#engines + 1] = d end
end
check(#engines == 4, "stacking on a ring member auto-mirrors (engines=" .. #engines .. ")")
if #engines == 4 then
  local gid = engines[1].sym
  local parents = {}
  local ok = true
  for _, d in ipairs(engines) do
    if d.sym ~= gid or gid == 0 then ok = false end
    if parents[d.parent] then ok = false end -- one engine PER decoupler
    parents[d.parent] = true
  end
  check(ok, "mirrored engines form their own ring, one per decoupler")
end

-- 5. Ring scrap: DEL on one engine takes all four; CTRL+Z brings them back.
local eng_node
for _, n in ipairs(nodes) do
  if n.valid and n.name == "PartEngineS" then eng_node = n break end
end
mouse_to_world(eng_node.x, eng_node.y, eng_node.z)
step(1)
press("delete")
step(2)
check(env.partCount == 6, "DEL on a ring member scraps the whole ring (partCount=" .. tostring(env.partCount) .. ")")
API.KEYS.down["ctrl"] = true
press("z")
step(2)
API.KEYS.down["ctrl"] = false
step(2)
check(env.partCount == 10, "CTRL+Z restores the scrapped ring (partCount=" .. tostring(env.partCount) .. ")")

-- ── verdict ──────────────────────────────────────────────────────────────────
if #failures == 0 then
  print("BUILDER SMOKE OK — symmetry ring, auto-mirror, ring scrap, undo")
  os.exit(0)
else
  print("BUILDER SMOKE FAILURES:")
  for _, f in ipairs(failures) do print("  ✗ " .. f) end
  os.exit(1)
end
