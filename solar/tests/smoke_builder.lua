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
              pitch = 0, roll = 0, yaw = 0, text = "", components = {},
              ui_rect = nil }
  n.getcomponent = function(self, kind) return self.components[kind] end
  -- node:uiRect() → x,y,w,h physical px (0,0,0,0 when it has no rect). The
  -- StagePanel gets a real rect so the panel hit-test can be exercised.
  n.uiRect = function(self)
    local r = self.ui_rect
    if not r then return 0, 0, 0, 0 end
    return r[1], r[2], r[3], r[4]
  end
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
  screenSize = function() return 1920, 1080 end,
  screenToRay = function(mx, my)
    return (mx - 500) / 100, 10.0, 0.0, 0.0, -1.0, 0.0
  end,
}

-- Raycast returns nothing — the builder's part_under_cursor then falls back
-- to the nearest projected part center, which the ortho camera resolves
-- exactly (worldToScreen carries z), so aiming the mouse at a part's
-- projected center selects it.
function API.raycast() return nil end
API.draw = {
  line = function() end, ring = function() end,
  sphere = function() end, box = function() end,
  cone = function() end, disc = function() end, tri = function() end,
}
API.save = {
  get = function(k) return save_store[k] end,
  set = function(k, v) save_store[k] = v end,
  flush = function() end,
}
API.scene = { load = function() end }

-- Audio: the builder plays UI clicks + a hangar bed; hand back a handle with
-- the SoundHandle surface (stop() is used on the bed) so it all no-ops here.
API.SOUNDS = {}
API.audio = {
  play = function(clip)
    API.SOUNDS[#API.SOUNDS + 1] = clip
    return {
      stop = function() end, pause = function() end, resume = function() end,
      setVolume = function() end, setPitch = function() end, setPan = function() end,
      setTrack = function() end, setPosition = function() end, seek = function() end,
      isPlaying = function() return true end, position = function() return 0.0 end,
    }
  end,
  track = function()
    return { setVolume = function() end, setPan = function() end,
      setMuted = function() end, setSoloed = function() end }
  end,
  stopAll = function() end,
}

-- ── load the builder ─────────────────────────────────────────────────────────
make_node("BuildStats", 0, 0, 0)
make_node("BuildHint", 0, 0, 0)
local stage_panel = make_node("StagePanel", 0, 0, 0)
stage_panel.components["UiElement"] = { visible = false }
-- The panel's solved screen rect (physical px) — what node:uiRect() returns.
stage_panel.ui_rect = { 16, 200, 250, 320 }

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
-- LEFTOVER GHOST YAW must not skew the disc (the round-12 "decouplers face
-- the wrong direction" bug: the outward solve assumed yaw 0 while the ghost
-- kept the user's R presses — 6 taps = 90° wrong).
for _ = 1, 6 do
  press("r")
  step(1)
end
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
  -- ORIENTATION: each disc's stack axis (its local +Y under YXZ euler) must
  -- point radially OUTWARD — the flat face presses against the hull.
  for _, d in ipairs(ring) do
    local cy, sy = math.cos(d.yaw or 0), math.sin(d.yaw or 0)
    local cx, sx = math.cos(d.pitch or 0), math.sin(d.pitch or 0)
    local cz, sz = math.cos(d.roll or 0), math.sin(d.roll or 0)
    local ax = -cy * sz + sy * sx * cz
    local ay = cx * cz
    local az = sy * sz + cy * sx * cz
    local r = math.sqrt(d.x * d.x + d.z * d.z)
    local dot = (d.x / r) * ax + (d.z / r) * az
    check(math.abs(dot) > 0.99 and math.abs(ay) < 0.05,
      string.format("disc axis outward (dot=%.2f, up-component=%.2f, yaw=%.2f pitch=%.2f roll=%.2f)",
        dot, ay, d.yaw or 0, d.pitch or 0, d.roll or 0))
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

-- 5. SELECT then DEL: click one engine to SELECT it (click no longer grabs),
--    DEL scraps its whole ring; CTRL+Z brings them back.
local eng_node
for _, n in ipairs(nodes) do
  if n.valid and n.name == "PartEngineS" then eng_node = n break end
end
mouse_to_world(eng_node.x, eng_node.y, eng_node.z)
click() -- selects the engine (no grab)
check(eng_node.x == (function()
  for _, n in ipairs(nodes) do if n.name == "PartEngineS" then return n.x end end
end)(), "clicking a part does not move it")
press("delete")
step(2)
check(env.partCount == 6, "DEL on a selected ring member scraps the whole ring (partCount=" .. tostring(env.partCount) .. ")")
API.KEYS.down["ctrl"] = true
press("z")
step(2)
API.KEYS.down["ctrl"] = false
step(2)
check(env.partCount == 10, "CTRL+Z restores the scrapped ring (partCount=" .. tostring(env.partCount) .. ")")

-- 6. GIZMO drag (Move tool): SELECT the pod, then grab its green Y-arrow and
--    pull up — the pod (and its stack) rises; CTRL+Z restores it.
local pod_node
for _, n in ipairs(nodes) do
  if n.valid and n.name == "PartPod" then pod_node = n end
end
press("1") step(1) -- Move tool (default, but be explicit)
mouse_to_world(pod_node.x, pod_node.y, pod_node.z)
click() -- SELECT the pod (gizmo now shows on it)
local y_before = pod_node.y
-- Aim mid-shaft of the Y arrow (len ≈ 1.15 + 0.5·reach; grab-tested s=4..10).
local glen = 1.15 + 0.5 * 0.5
mouse_to_world(pod_node.x, pod_node.y + glen * 0.7, pod_node.z)
step(1)
MOUSE.lmb = true
step(1) -- grab the arrow
MOUSE.y = MOUSE.y - 60 -- drag up (screen y flipped: up = smaller y)
step(1)
MOUSE.lmb = false
step(14)
check(pod_node.y > y_before + 0.3,
  string.format("Move-tool Y-arrow drag lifts the part (%.2f → %.2f)", y_before, pod_node.y))
API.KEYS.down["ctrl"] = true
press("z")
step(2)
API.KEYS.down["ctrl"] = false
check(math.abs(pod_node.y - y_before) < 0.01, "CTRL+Z undoes the gizmo drag")

-- 7. STAGING (Stage tool): an axial decoupler under the tank makes a second
--    event; default order = ring first. Enter the Stage tool, drag row #1
--    below row #2 via the panel's REAL uiRect, and the saved blueprint
--    carries the swapped order.
env.pick("decoupler")
step(12)
mouse_to_world(tank_x, tank_y - 0.5, tank_z) -- tank's bottom node
step(1)
click()
local stages0 = {}
for _, d in ipairs(saved_parts()) do
  if d.decouple == 1 then stages0[d.id] = d.stage end
end
check(stages0.radialDec == 1 and stages0.decoupler == 2,
  string.format("default staging: ring first (ring=%s, axial=%s)",
    tostring(stages0.radialDec), tostring(stages0.decoupler)))
press("3") step(2) -- Stage tool: the panel appears + becomes interactive
check(stage_panel.components["UiElement"].visible == true,
  "the staging panel shows in the Stage tool")
-- Rows laid out under 3 header lines inside the rect {16,200,250,320}.
local rx, ry, rw, rh = 16, 200, 250, 320
local n_rows = 3 + 2
local pad = rh * 0.03
local pitch = (rh - pad * 2) / n_rows
local row_y = function(r) return ry + pad + (3 + r - 1) * pitch + pitch * 0.5 end
MOUSE.x, MOUSE.y = rx + 40, row_y(1)
step(1)
MOUSE.lmb = true
step(1)
MOUSE.y = row_y(2)
step(1)
MOUSE.lmb = false
step(16)
local stages1 = {}
for _, d in ipairs(saved_parts()) do
  if d.decouple == 1 then stages1[d.id] = d.stage end
end
check(stages1.radialDec == 2 and stages1.decoupler == 1,
  string.format("dragging row #1 below row #2 swaps the firing order (ring=%s, axial=%s)",
    tostring(stages1.radialDec), tostring(stages1.decoupler)))

-- 8. FINS are SIDE-MOUNT blades now (no axial nodes) that self-orient outward
--    and ring with symmetry — Ty's "a side mount fin you attach with symmetry".
--    Symmetry is still ×4 from step 3; drop a fin on the pod's free +Z flank and
--    the ring must place four, each blade's local +X pointing radially OUTWARD.
press("1") step(2) -- back to the Move tool (Stage tool owns the panel)
local n_before = env.partCount
env.pick("fins")
step(12)
mouse_to_world(pod_node.x, pod_node.y, pod_node.z + 0.5) -- the pod's +Z flank node
step(1)
click()
step(2)
check(env.partCount == n_before + 4, string.format(
  "a fin placed with ×4 symmetry rings four blades (partCount %d→%d)", n_before, env.partCount))
local fins, pod_bp = {}, nil
for _, d in ipairs(saved_parts()) do
  if d.id == "fins" then fins[#fins + 1] = d end
  if d.id == "pod" then pod_bp = d end
end
check(#fins == 4, "blueprint carries four fins (" .. #fins .. ")")
-- The fins ring the POD (their host), so outward is measured from the pod's XZ.
local all_outward = #fins == 4 and pod_bp ~= nil
for _, f in ipairs(fins) do
  -- blade local +X under yaw (pitch/roll 0): (cos yaw, 0, -sin yaw).
  local lx, lz = math.cos(f.yaw), -math.sin(f.yaw)
  local ox, oz = f.x - pod_bp.x, f.z - pod_bp.z
  local ol = math.sqrt(ox * ox + oz * oz)
  if ol > 1e-4 then
    local dot = (lx * ox + lz * oz) / ol
    if dot < 0.9 then all_outward = false end
  end
end
check(all_outward, "every fin blade points radially outward from its flank")

-- ── verdict ──────────────────────────────────────────────────────────────────
if #failures == 0 then
  print("BUILDER SMOKE OK — select+tools, symmetry, auto-mirror, gizmo drag, stage reorder, side fins")
  os.exit(0)
else
  print("BUILDER SMOKE FAILURES:")
  for _, f in ipairs(failures) do print("  ✗ " .. f) end
  os.exit(1)
end
