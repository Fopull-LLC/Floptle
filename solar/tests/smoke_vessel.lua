-- SMOKE HARNESS: runs the real vessel_spawner.lua + vessel_controller.lua
-- against a stubbed engine API and drives the whole launch sequence offline —
-- spawn, assemble, clamp, board, release, thrust, flames, stage, EVA,
-- re-board from the ground. Any Lua runtime error in the scripts fails loudly
-- with a traceback, and the end-state assertions catch logic regressions.
--
--   luajit solar/tests/smoke_vessel.lua
--
-- Fidelity notes (bugs this stub is shaped to catch):
--  * node.x/y/z are PARENT-LOCAL, exactly like the engine — writing a world
--    number into a parented node's coords lands it in the wrong place (the
--    round-7 far-side launchpad).
--  * The planet MOVES on rails; planet-parented nodes ride it through the
--    hierarchy, the anchored assembly rides via the carry — anything
--    positioned in stale world coords drifts and fails the proximity checks.

local T = 0.0
local TICK = 1 / 60

-- ── node registry (parent-LOCAL coordinates, like the engine) ───────────────
local nodes = {}
local next_id = 1

local function make_node(name, x, y, z, parent)
  local n = {
    __id = next_id, name = name, valid = true, visible = true,
    x = x or 0, y = y or 0, z = z or 0,
    pitch = 0, roll = 0, yaw = 0,
    vx = 0, vy = 0, vz = 0,
    up_x = 0, up_y = 1, up_z = 0, grounded = false,
    components = {}, kids = {}, parent = parent,
    shader_params = {}, particles_state = nil,
  }
  n.children = function(self)
    local out = {}
    for _, k in ipairs(self.kids) do out[#out + 1] = k end
    return out
  end
  n.getcomponent = function(self, kind) return self.components[kind] end
  n.setShaderParam = function(self, key, a, b, c) self.shader_params[key] = { a, b, c } end
  n.particles = function(self)
    if not self.particles_state then return nil end
    local st = self.particles_state
    return {
      isPlaying = function() return st.playing end,
      play = function() st.playing = true end,
      stop = function() st.playing = false end,
      setIntensity = function(_, v) st.intensity = v end,
    }
  end
  if parent then parent.kids[#parent.kids + 1] = n end
  next_id = next_id + 1
  nodes[#nodes + 1] = n
  return n
end

local function world_of(n)
  local x, y, z = n.x, n.y, n.z
  local p = n.parent
  while p do
    x, y, z = x + p.x, y + p.y, z + p.z
    p = p.parent
  end
  return x, y, z
end

local function find_node(name)
  for _, n in ipairs(nodes) do
    if n.valid and n.name == name then return n end
  end
  return nil
end

-- ── world state ─────────────────────────────────────────────────────────────
-- One planet on rails: it moves +X at 90 u/s. Everything must be positioned
-- relative to it or drift bugs show as proximity failures.
local planet = { name = "Golil", x = 5000, y = 0, z = 0, soi = 30000, mu = 4.9e5, radius = 220 }
local planet_node = make_node("Golil", planet.x, planet.y, planet.z)

local save_store = {}
local logs = {}
local spawn_queue = {}
local asm = nil
local force_calls = 0
local torque_calls = 0

-- ── stub API ────────────────────────────────────────────────────────────────
local API = {}

function API.vec3(x, y, z) return { x = x or 0, y = y or 0, z = z or 0 } end
function API.distance(a, b)
  local dx, dy, dz = a.x - b.x, a.y - b.y, a.z - b.z
  return math.sqrt(dx * dx + dy * dy + dz * dz)
end
function API.log(msg) logs[#logs + 1] = tostring(msg) end
API.print = API.log
function API.find(name) return find_node(name) end
function API.destroy(n) n.valid = false end

API.KEYS = { down = {}, edge = {} }
API.input = {
  key = function(k) return API.KEYS.down[k] or false end,
  pressed = function(k) return API.KEYS.edge[k] or false end,
  button = function() return false end,
  mouse = function() return 0, 0 end,
}

API.save = {
  get = function(k) return save_store[k] end,
  set = function(k, v) save_store[k] = v end,
  flush = function() end,
  slot = function() return "slot1" end,
}

API.space = {
  time = function() return T end,
  warp = function(m) return 1 end,
  bodies = function() return { planet } end,
  body = function(name) return name == planet.name and planet or nil end,
  dominant = function() return planet.name end,
  elements = function()
    if T < 3.0 then return nil end
    return { body = planet.name, apoapsis = 400, periapsis = 250, period = 950 }
  end,
}

API.terrain = { warm = function() end, query = function() return 0.1 end }
API.draw = {
  ring = function() end, line = function() end,
  sphere = function() end, box = function() end,
}
API.physics = { pause = function() end, isPaused = function() return false end }

function API.raycast(ox, oy, oz)
  local dx, dy, dz = ox - planet.x, oy - planet.y, oz - planet.z
  local d = math.sqrt(dx * dx + dy * dy + dz * dz)
  return { distance = d - planet.radius, node = planet_node }
end

function API.spawn(prefab, pos, cb, parent)
  spawn_queue[#spawn_queue + 1] = { prefab = prefab, pos = pos, cb = cb, parent = parent }
end

API.assembly = {
  info = function(node)
    if not (asm and asm.root == node) then return nil end
    -- `origin` is the PHYSICS-fresh pose: model the rails-carry gap by
    -- reporting the node pos PLUS this tick's carry delta — controllers must
    -- base force/pod math on it, never on the (stale) node coords.
    return {
      mass = 8.0,
      com = { x = node.x + asm.carry_dx, y = node.y, z = node.z },
      origin = { x = node.x + asm.carry_dx, y = node.y, z = node.z },
      vel = { x = asm.vel.x, y = asm.vel.y, z = asm.vel.z },
      angVel = { x = 0, y = 0, z = 0 },
      grounded = asm.anchored,
      anchored = asm.anchored,
      parts = asm.parts,
    }
  end,
  rebuild = function(node)
    local parts = {}
    for _, k in ipairs(node.kids) do parts[#parts + 1] = k.__id end
    asm = { root = node, parts = parts, anchored = false,
            vel = { x = 0, y = 0, z = 0 }, carry_dx = 0 }
  end,
  setAnchored = function(node, on)
    assert(asm and asm.root == node, "setAnchored before rebuild")
    asm.anchored = on
  end,
  teleport = function(node, pos)
    assert(asm and asm.root == node, "teleport before rebuild")
    node.x, node.y, node.z = pos.x, pos.y, pos.z
  end,
  forceAt = function(node, force, at)
    force_calls = force_calls + 1
    -- The application point must be PHYSICS-consistent: computed from
    -- info.origin (node pos + carry), never the stale node pose. An offset
    -- of a carry-delta here was the constant SAS torque bias of round 10.
    if asm and asm.root == node and asm.carry_dx ~= 0 then
      local expect_x = node.x + asm.carry_dx
      if math.abs(at.x - expect_x) > 0.5 then
        error(string.format(
          "forceAt point off the physics hull: at.x=%.2f, expected ~%.2f (stale node pose?)",
          at.x, expect_x))
      end
    end
  end,
  force = function() force_calls = force_calls + 1 end,
  torque = function() torque_calls = torque_calls + 1 end,
  impulseAt = function() end,
  split = function(node, parts, cb) end,
}

-- ── script loading ──────────────────────────────────────────────────────────
local script_envs = {}

function API.findScript(kind)
  local env = script_envs[kind]
  if not env then return nil end
  return setmetatable({ node = env.__node }, { __index = env })
end
API.findScripts = function(kind)
  local s = API.findScript(kind)
  return s and { s } or {}
end

local function load_script(path, kind, node)
  local env = setmetatable({}, { __index = function(_, k)
    if k == "time" then return T end
    return API[k] or _G[k]
  end })
  env.__node = node
  env.__kind = kind
  local chunk = assert(loadfile(path))
  setfenv(chunk, env)
  chunk()
  env.params = {}
  for k, v in pairs(env.defaults or {}) do env.params[k] = v end
  script_envs[kind] = env
  return env
end

local failures = {}
local function call(env, fn, ...)
  local f = env[fn]
  if not f then return end
  local ok, err = xpcall(f, debug.traceback, ...)
  if not ok then
    failures[#failures + 1] = string.format("%s.%s: %s", env.__kind or "?", fn, err)
  end
end

-- ── the simulated engine loop ───────────────────────────────────────────────
local spawner_env, controller_env

local function deliver_spawns()
  local batch = spawn_queue
  spawn_queue = {}
  for _, req in ipairs(batch) do
    -- Like the engine: spawn at the WORLD pos, then re-express as
    -- parent-local when a parent is given.
    local x, y, z = req.pos.x, req.pos.y, req.pos.z
    if req.parent then
      local pwx, pwy, pwz = world_of(req.parent)
      x, y, z = x - pwx, y - pwy, z - pwz
    end
    local n = make_node(req.prefab, x, y, z, req.parent)
    -- Engine part prefabs carry an "Engine Flame" vfx child.
    if req.prefab:find("PartEngine") then
      local fl = make_node("Engine Flame", 0, -0.7, 0, n)
      fl.particles_state = { playing = false, intensity = 0 }
      fl.components["PointLight"] = { intensity = 0 }
    end
    if req.prefab == "Vessel" then
      controller_env = load_script("solar/scripts/vessel_controller.lua", "vessel_controller", n)
      call(controller_env, "start", n)
    end
    if req.cb then
      local ok, err = xpcall(req.cb, debug.traceback, n)
      if not ok then failures[#failures + 1] = "spawn cb: " .. err end
    end
  end
end

local function step(n)
  for _ = 1, n do
    T = T + TICK
    -- Rails: the planet moves; parented nodes ride via the hierarchy, the
    -- anchored assembly rides via the engine's carry (modeled here).
    local dx = 90 * TICK
    planet.x = planet.x + dx
    planet_node.x = planet_node.x + dx
    -- Model the fixedUpdate rails-carry gap: the sim carries the compound at
    -- the TOP of the tick but the node writes back after scripts run —
    -- info.origin is ahead of node.x by one carry delta during the script
    -- pass, and the post-tick writeback closes it.
    if asm then asm.carry_dx = dx end
    deliver_spawns()
    if spawner_env then call(spawner_env, "update", spawner_env.__node, TICK) end
    if controller_env then call(controller_env, "fixedUpdate", controller_env.__node, TICK) end
    -- Post-tick writeback closes the gap, then the camera pass runs.
    if asm then
      asm.root.x = asm.root.x + asm.carry_dx
      asm.carry_dx = 0
    end
    if controller_env then call(controller_env, "lateUpdate", controller_env.__node, TICK) end
    API.KEYS.edge = {}
  end
end

local function press(k) API.KEYS.edge[k] = true end

local function check(cond, what)
  if not cond then failures[#failures + 1] = "CHECK FAILED: " .. what end
end

-- ── the scenario ────────────────────────────────────────────────────────────
save_store["shipyard.blueprint"] = { parts = {
  { uid = 1, id = "pod", prefab = "PartPod", x = 0, y = 3.0, z = 0, yaw = 0,
    h = 0.8, mass = 1.2, cost = 400, kind = "crewed", thrust = 0, burn = 0, fuel = 0, decouple = 0, legs = 0 },
  { uid = 2, id = "tankS", prefab = "PartTankS", x = 0, y = 2.1, z = 0, yaw = 0,
    h = 1.0, mass = 1.5, cost = 120, kind = "tank", thrust = 0, burn = 0, fuel = 60, decouple = 0, legs = 0 },
  { uid = 3, id = "decoupler", prefab = "PartDecoupler", x = 0, y = 1.47, z = 0, yaw = 0,
    h = 0.25, mass = 0.15, cost = 60, kind = "structural", thrust = 0, burn = 0, fuel = 0, decouple = 1, legs = 0 },
  { uid = 4, id = "engineS", prefab = "PartEngineS", x = 0, y = 0.65, z = 0, yaw = 0,
    h = 1.3, mass = 0.8, cost = 150, kind = "engine", thrust = 55, burn = 0.9, fuel = 0, decouple = 0, legs = 0 },
  -- An UPPER-stage engine above the decoupler: it must stay cold (no thrust,
  -- no flame) until the stage below separates.
  { uid = 5, id = "engineS", prefab = "PartEngineS", x = 0, y = 2.5, z = 0, yaw = 0,
    h = 1.3, mass = 0.8, cost = 150, kind = "engine", thrust = 55, burn = 0.9, fuel = 0, decouple = 0, legs = 0 },
} }
save_store["shipyard.launch"] = 1
save_store["shipyard.pilot"] = 1

local astro = make_node("Astronaut", planet.x, planet.y + planet.radius + 6, planet.z)
make_node("Ship", planet.x, planet.y + planet.radius + 4, planet.z)
for _, nm in ipairs({ "Ship HUD Text", "Vessel HUD", "Navball", "Speed Tape",
                      "Alt Tape", "Speed Readout", "Alt Readout", "Heading Readout" }) do
  local n = make_node(nm, 0, 0, 0)
  n.components["UiElement"] = { visible = false }
  n.text = ""
end

local sp_node = make_node("Vessel Spawner", 0, 0, 0)
spawner_env = load_script("solar/scripts/vessel_spawner.lua", "vessel_spawner", sp_node)
call(spawner_env, "start", sp_node)

step(30) -- spawn pad + vessel + parts, rebuild, clamp, board

check(asm ~= nil, "assembly was rebuilt")
check(asm and asm.anchored, "vessel is CLAMPED after assembly")
check(controller_env and controller_env.piloting, "pilot auto-boarded from launch")
check(save_store["shipyard.pilot"] == 0, "pilot handoff flag consumed")
check(astro.visible == false, "astronaut rides hidden in the pod")

-- The pad must be planet-parented and physically UNDER the vessel in WORLD
-- coordinates (round 7: its local position had the planet offset added twice,
-- putting it 5000 units away).
local pad = find_node("Launchpad")
check(pad ~= nil, "launchpad spawned")
if pad and asm then
  check(pad.parent == planet_node, "launchpad parented to the planet")
  local pwx, pwy, pwz = world_of(pad)
  local vwx, vwy, vwz = world_of(asm.root)
  local d = math.sqrt((pwx - vwx) ^ 2 + (pwy - vwy) ^ 2 + (pwz - vwz) ^ 2)
  check(d < 12, string.format("launchpad sits under the vessel (d=%.1f)", d))
end

local hudn = find_node("Ship HUD Text")
check(hudn and hudn.text ~= "" and hudn.text:find("CLAMPED") ~= nil,
  "HUD shows the CLAMPED state (got: " .. tostring(hudn and hudn.text) .. ")")

-- Release, throttle: thrust + flames must fire.
press("space")
step(2)
check(asm and asm.anchored == false, "SPACE released the launch clamps")
API.KEYS.down["shift"] = true
step(30)
API.KEYS.down["shift"] = false
check(force_calls > 0, "throttle produced engine forceAt calls")
-- STAGE GATING: only the BOTTOM stage's single engine may fire — two engines
-- thrusting doubles the per-tick call count (30 ticks → ~30 calls, not ~60).
check(force_calls <= 35,
  string.format("upper-stage engine stayed cold before staging (forceAt ×%d over 30 ticks)", force_calls))
check(torque_calls > 0, "attitude loop produced torque calls")
check(controller_env.throttle > 0, "throttle climbed under SHIFT")
local flame_lo, flame_hi
for _, n in ipairs(nodes) do
  if n.name == "Engine Flame" and n.parent then
    if n.parent.y < 1.5 then flame_lo = n else flame_hi = n end
  end
end
check(flame_lo ~= nil and flame_hi ~= nil, "both engine prefabs spawned flame children")
check(flame_lo and flame_lo.particles_state.playing, "bottom-stage plume plays under throttle")
check(flame_lo and flame_lo.components["PointLight"].intensity > 0, "plume light follows throttle")
check(flame_hi and not flame_hi.particles_state.playing, "upper-stage plume stays COLD before staging")

-- Stage: SPACE now fires the decoupler path (split stubbed; must not error).
press("space")
step(2)

-- EVA: F steps out (flames off, HUD hides); board again FROM THE GROUND —
-- the pod is ~3 units up, so boarding must measure to the vessel's spine.
press("f")
step(2)
check(controller_env.piloting == false, "F exits the pod")
check(astro.visible == true, "astronaut visible after EVA exit")
check(flame_lo and flame_lo.particles_state.playing == false, "plume stops on EVA exit")
check(flame_hi and flame_hi.particles_state.playing == false, "upper plume off on EVA exit")
local hud_el = hudn.components["UiElement"]
check(hud_el.visible == false, "HUD hidden on EVA")
astro.x = asm.root.x + 2.0 -- standing beside the stack, at BASE height
astro.y = asm.root.y
astro.z = asm.root.z
press("f")
step(2)
check(controller_env.piloting == true, "F re-boards from the ground (spine reach)")
step(10)
check(hudn.text ~= "" and hud_el.visible ~= false, "HUD repaints after re-board")

-- ── verdict ─────────────────────────────────────────────────────────────────
if #failures == 0 then
  print(string.format("SMOKE OK — %d ticks, %d logs", math.floor(T / TICK + 0.5), #logs))
  os.exit(0)
else
  print("SMOKE FAILURES:")
  for _, f in ipairs(failures) do print("  ✗ " .. f) end
  print("\n-- script logs --")
  for _, l in ipairs(logs) do print("  " .. l) end
  os.exit(1)
end
