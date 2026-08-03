-- SMOKE HARNESS for base_facilities.lua: runs the real script against a stubbed
-- engine API (a round, offset planet + a crew node on its surface) and drives the
-- placement to completion. Catches Lua runtime errors and the classic sins this
-- kind of surface-anchoring code commits:
--   * writing WORLD coords into a planet-parented node (the round-7 far-side bug):
--     every facility's local coords must be BODY-RELATIVE, not absolute.
--   * facilities seated off the ground / flung away by a bad raycast.
--   * not all facilities placed (an angle that never finds ground).
--   * the base MOVING between sessions — it is sited once, saved body-relative,
--     and rebuilt on the same ground however far the crew has wandered.
--   * anything sited while the loading screen is still up (the crew is parked
--     over the north pole then, not standing where they live).
--
--   luajit solar/tests/smoke_base.lua

local TICK = 1 / 60

-- Planet OFFSET from the origin so a "local == world" bug can't accidentally pass.
local planet = { name = "Athosil", x = 300, y = -120, z = 80, radius = 200, mu = 4.0e5, soi = 5000 }
local planet_node = { __id = 1, name = planet.name, x = planet.x, y = planet.y, z = planet.z, valid = true }

-- Crew on the +Y-ish surface of the planet (a point at radius on the surface).
local up0 = { x = 0.12, y = 0.97, z = 0.21 }
local ul = math.sqrt(up0.x^2 + up0.y^2 + up0.z^2)
up0.x, up0.y, up0.z = up0.x / ul, up0.y / ul, up0.z / ul
local crew = {
  __id = 2, name = "Astronaut", valid = true,
  x = planet.x + up0.x * planet.radius,
  y = planet.y + up0.y * planet.radius,
  z = planet.z + up0.z * planet.radius,
}

local nodes = { planet_node, crew }
local spawned = {}
local next_id = 100

_G.vec3 = function(x, y, z) return { x = x, y = y, z = z } end
_G.log = function(_) end
_G.find = function(name)
  for _, n in ipairs(nodes) do if n.name == name then return n end end
  return nil
end
_G.terrain = { warm = function() end }
_G.space = {
  bodies = function() return { planet } end,
  body = function(name) if name == planet.name then return planet end end,
}
local STORE = {}
_G.save = { get = function(k) return STORE[k] end, set = function(k, v) STORE[k] = v end }
-- The loading screen: while `loading` is true nothing may be sited.
local gm = { loading = false }
_G.findScript = function(kind) if kind == "game_manager" then return gm end end

-- Faithful ray/sphere intersection against the planet.
_G.raycast = function(ox, oy, oz, dx, dy, dz, maxd)
  local ocx, ocy, ocz = ox - planet.x, oy - planet.y, oz - planet.z
  local b = 2 * (dx * ocx + dy * ocy + dz * ocz)
  local c = ocx^2 + ocy^2 + ocz^2 - planet.radius^2
  local disc = b * b - 4 * c
  if disc < 0 then return nil end
  local t = (-b - math.sqrt(disc)) / 2
  if t < 0 or t > (maxd or 1e9) then return nil end
  return { distance = t }
end

_G.spawn = function(prefab, pos, cb, parent)
  local inst = { __id = next_id, name = prefab, valid = true,
    x = pos.x, y = pos.y, z = pos.z, pitch = 0, roll = 0, yaw = 0, parent = parent }
  next_id = next_id + 1
  nodes[#nodes + 1] = inst
  if cb then cb(inst) end
  spawned[#spawned + 1] = { prefab = prefab, world = { x = pos.x, y = pos.y, z = pos.z }, inst = inst }
  return inst
end

-- ── run ─────────────────────────────────────────────────────────────────────
local function run_session(host_name, limit)
  local chunk = assert(loadfile("solar/scripts/base_facilities.lua"))
  chunk() -- fresh script state (its `done`/`placed` locals reset)
  assert(type(start) == "function", "base_facilities must define start()")
  assert(type(update) == "function", "base_facilities must define update()")
  start(crew)
  local host = { __id = 3, name = host_name }
  for _ = 1, (limit or 6000) do
    update(host, TICK)
    if #spawned >= 5 then return true end
  end
  return false
end

-- Session 1, still loading: the crew is parked over the pole, nothing goes up.
gm.loading = true
assert(not run_session("Base Facilities", 240), "sited during the loading screen")
assert(#spawned == 0, "expected nothing sited while loading, got " .. #spawned)
assert(STORE["base.body"] == nil, "the base site must not be recorded while loading")

-- Session 1, loaded: the colony goes up around the crew and is remembered.
gm.loading = false
assert(run_session("Base Facilities"), "base placement never completed (100s of ticks)")

-- ── assertions ────────────────────────────────────────────────────────────
assert(#spawned == 5, "expected 5 facilities, got " .. #spawned)

local seen = {}
for _, s in ipairs(spawned) do
  seen[s.prefab] = true
  -- 1. Parented to the planet node.
  assert(s.inst.parent == planet_node, s.prefab .. " must be parented to the planet")
  -- 2. Local coords are BODY-RELATIVE, not world. World pos ≈ body + local.
  local lx, ly, lz = s.inst.x, s.inst.y, s.inst.z
  local wx, wy, wz = planet.x + lx, planet.y + ly, planet.z + lz
  local dw = math.sqrt((wx - s.world.x)^2 + (wy - s.world.y)^2 + (wz - s.world.z)^2)
  assert(dw < 1e-3, s.prefab .. " local coords are NOT body-relative (far-side bug): off by " .. dw)
  -- 3. Seated near the surface (origin within a few units above ground).
  local r = math.sqrt((wx - planet.x)^2 + (wy - planet.y)^2 + (wz - planet.z)^2)
  local above = r - planet.radius
  assert(above > 0.2 and above < 6.0, s.prefab .. " badly seated: " .. above .. " above surface")
  -- 4. Near the base, not flung across the system.
  local db = math.sqrt((wx - crew.x)^2 + (wy - crew.y)^2 + (wz - crew.z)^2)
  assert(db < 60, s.prefab .. " sited too far from base: " .. db)
end
for _, want in ipairs({ "FacCommand", "FacHangar", "FacPower", "FacTracking", "FacDepot" }) do
  assert(seen[want], "missing facility: " .. want)
end

-- 5. The site is remembered BODY-RELATIVE (absolute coords go stale on an orbit).
assert(STORE["base.body"] == planet.name, "the base site's body was not saved")
local hx, hy, hz = STORE["base.x"], STORE["base.y"], STORE["base.z"]
assert(hx and hy and hz, "the base site offset was not saved")
local hr = math.sqrt(hx * hx + hy * hy + hz * hz)
assert(math.abs(hr - planet.radius) < 2.0,
  string.format("the saved site is not on the surface (r = %.1f)", hr))
local first = { x = planet.x + hx, y = planet.y + hy, z = planet.z + hz }

-- ── session 2: reload, crew 300 m away, the base must NOT follow them ───────
local before = #spawned
-- The planet has moved on its orbit AND the crew wakes up somewhere else.
planet.x, planet.y, planet.z = planet.x + 5000, planet.y - 800, planet.z + 1200
planet_node.x, planet_node.y, planet_node.z = planet.x, planet.y, planet.z
local away = { x = 0.62, y = 0.70, z = 0.35 }
local al = math.sqrt(away.x^2 + away.y^2 + away.z^2)
crew.x = planet.x + away.x / al * planet.radius
crew.y = planet.y + away.y / al * planet.radius
crew.z = planet.z + away.z / al * planet.radius
spawned = {}
assert(run_session("Base Facilities"), "the base never rebuilt in session 2")
assert(#spawned == 5, "session 2 expected 5 facilities, got " .. #spawned)

-- Same ground, moved only by the planet's own orbit: the saved offset is what
-- carries the colony, so it must be UNCHANGED and the buildings must land on it.
assert(STORE["base.x"] == hx and STORE["base.y"] == hy and STORE["base.z"] == hz,
  "session 2 overwrote the saved base site")
local site2 = { x = planet.x + hx, y = planet.y + hy, z = planet.z + hz }
for _, s in ipairs(spawned) do
  local d = math.sqrt((s.world.x - site2.x)^2 + (s.world.y - site2.y)^2 + (s.world.z - site2.z)^2)
  assert(d < 60, s.prefab .. " rebuilt " .. math.floor(d) .. " m from the saved site")
end
local crew_d = math.sqrt((crew.x - site2.x)^2 + (crew.y - site2.y)^2 + (crew.z - site2.z)^2)
assert(crew_d > 100, "the test crew should be far from home (got " .. math.floor(crew_d) .. " m)")

print(string.format(
  "smoke_base OK — 5 facilities sited, body-relative, seated; base stayed home across a reload (crew %.0f m away)",
  crew_d))
