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
    __id = next_id, id = next_id, name = name, valid = true, visible = true,
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
local planet = { name = "Golil", x = 5000, y = 0, z = 0, soi = 30000, mu = 4.9e5, radius = 220,
                 vx = 90, vy = 0, vz = 0 }
local planet_node = make_node("Golil", planet.x, planet.y, planet.z)

local save_store = {}
local logs = {}
local spawn_queue = {}
local asm = nil
local force_calls = 0
local torque_calls = 0
local split_calls = {}

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
  mouse_delta = function() return 0, 0 end,
  scroll = function() return 0 end,
  setMouseLocked = function() end,
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
  -- SOI-honoring, like the engine: outside the planet's sphere of influence
  -- there is NO dominant body (the stashed scout parks out there on purpose).
  dominant = function(x, y, z)
    if not x then return planet.name end
    local dx, dy, dz = x - planet.x, y - planet.y, z - planet.z
    if dx * dx + dy * dy + dz * dz < planet.soi * planet.soi then return planet.name end
    return nil
  end,
  elements = function()
    if T < 3.0 then return nil end
    return { body = planet.name, apoapsis = 400, periapsis = 250, period = 950 }
  end,
  propagate = function(x, y, z, vx, vy, vz, mu, dt)
    return x + vx * dt, y + vy * dt, z + vz * dt, vx, vy, vz
  end,
  gravity = function(x, y, z)
    local dx, dy, dz = planet.x - x, planet.y - y, planet.z - z
    local r2 = dx * dx + dy * dy + dz * dz
    if r2 < 1 then return 0, 0, 0 end
    local r = math.sqrt(r2)
    local g = planet.mu / r2
    return dx / r * g, dy / r * g, dz / r * g
  end,
}

API.CRATERS = {}
API.terrain = { warm = function() end, query = function() return 0.1 end,
  dig = function(x, y, z, r) API.CRATERS[#API.CRATERS + 1] = { x = x, y = y, z = z, r = r } end }
-- Line recorder: update_map3d draws the subject craft's conic in cyan
-- (0.35, 0.85, 1.0) — the map assertions look for it here. Cleared per tick.
local tick_lines = {}
API.draw = {
  ring = function() end,
  line = function(x1, y1, z1, x2, y2, z2, r, g, b, a)
    tick_lines[#tick_lines + 1] = { r = r, g = g, b = b }
  end,
  sphere = function() end, box = function() end,
}
API.physics = { pause = function() end, isPaused = function() return false end }

-- Audio: record every clip played so we could assert on it; hand back a handle
-- with the full SoundHandle/TrackHandle surface the real API exposes so the
-- vessel's engine/reentry/bed loops and one-shots all no-op cleanly here.
API.SOUNDS = {}
local function sound_handle()
  return {
    stop = function() end, pause = function() end, resume = function() end,
    setVolume = function() end, setPitch = function() end, setPan = function() end,
    setTrack = function() end, setPosition = function() end, seek = function() end,
    isPlaying = function() return true end, position = function() return 0.0 end,
  }
end
API.audio = {
  play = function(clip)
    API.SOUNDS[#API.SOUNDS + 1] = clip
    return sound_handle()
  end,
  track = function()
    return { setVolume = function() end, setPan = function() end,
      setMuted = function() end, setSoloed = function() end }
  end,
  stopAll = function() end,
}

function API.raycast(ox, oy, oz)
  local dx, dy, dz = ox - planet.x, oy - planet.y, oz - planet.z
  local d = math.sqrt(dx * dx + dy * dy + dz * dz)
  return { distance = d - planet.radius, node = planet_node }
end

function API.spawn(prefab, pos, cb, parent)
  spawn_queue[#spawn_queue + 1] = { prefab = prefab, pos = pos, cb = cb, parent = parent }
end
API.EFFECTS = {}
function API.spawnEffect(name, x, y, z)
  API.EFFECTS[#API.EFFECTS + 1] = { name = name, x = x, y = y, z = z }
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
  keepLive = function(node, on)
    assert(asm and asm.root == node, "keepLive before rebuild")
    asm.kept_live = on
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
    -- Engines sit at known blueprint x-offsets (0 for the stack, 2.0 for
    -- the side booster) — the point must land on ONE of them, carried.
    if asm and asm.root == node and asm.carry_dx ~= 0 then
      local ok = false
      for _, ex in ipairs({ 0.0, 2.0 }) do
        if math.abs(at.x - (node.x + asm.carry_dx + ex)) < 0.5 then ok = true break end
      end
      if not ok then
        error(string.format(
          "forceAt point off the physics hull: at.x=%.2f, node.x=%.2f carry=%.2f (stale node pose?)",
          at.x, node.x, asm.carry_dx))
      end
    end
  end,
  force = function(node, f)
    force_calls = force_calls + 1
    API.LAST_FORCE = { x = f.x, y = f.y, z = f.z } -- through-CoM (chute drag)
  end,
  torque = function() torque_calls = torque_calls + 1 end,
  impulseAt = function() end,
  -- Faithful split: departing nodes leave the root's children (the engine
  -- re-parents them under a fresh stage root) and the compound's part list.
  split = function(node, parts, cb)
    split_calls[#split_calls + 1] = #parts
    for _, pn in ipairs(parts) do
      for i, k in ipairs(node.kids) do
        if k == pn then table.remove(node.kids, i) break end
      end
      pn.parent = nil
    end
    if asm and asm.root == node then
      local keep = {}
      for _, id in ipairs(asm.parts) do
        local gone = false
        for _, pn in ipairs(parts) do
          if pn.__id == id then gone = true break end
        end
        if not gone then keep[#keep + 1] = id end
      end
      asm.parts = keep
    end
  end,
  -- Injectable per-part contact loads (the engine's per-tick attribution):
  -- push { part=, impulse=, x=, y=, z= } into API.IMPACTS to simulate a slam.
  impacts = function(node)
    local out = API.IMPACTS or {}
    API.IMPACTS = {}
    return out
  end,
}

-- ── script loading ──────────────────────────────────────────────────────────
-- kind → LIST of envs, in registration order: findScript returns the FIRST
-- (arbitrary, like the engine), findScripts returns them all — scripts that
-- need THE piloted vessel must scan, and the decoy below proves they do.
local script_envs = {}

local function handle_of(env)
  return setmetatable({ node = env.__node }, { __index = env })
end

function API.findScript(kind)
  local l = script_envs[kind]
  if not l or #l == 0 then return nil end
  return handle_of(l[1])
end
API.findScripts = function(kind)
  local out = {}
  for _, env in ipairs(script_envs[kind] or {}) do out[#out + 1] = handle_of(env) end
  return out
end

local function register_script(kind, env)
  script_envs[kind] = script_envs[kind] or {}
  table.insert(script_envs[kind], env)
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
  register_script(kind, env)
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
local spawner_env, controller_env, scout_env

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
    -- The landing-leg prefab is a hierarchy: the gear device rolls the two
    -- EMPTY pivot nodes (hip → knee), so the harness must spawn them too.
    if req.prefab == "PartLegs" then
      local up = make_node("LegUpperPivot", 0.12, 0.0, 0.0, n)
      make_node("LegKneePivot", 0.0, -0.84, 0.0, up)
    end
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
    tick_lines = {}
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
    if scout_env then call(scout_env, "fixedUpdate", scout_env.__node, TICK) end
    if controller_env then call(controller_env, "fixedUpdate", controller_env.__node, TICK) end
    -- Post-tick writeback closes the gap, then the camera pass runs.
    if asm then
      asm.root.x = asm.root.x + asm.carry_dx
      asm.carry_dx = 0
    end
    if controller_env then call(controller_env, "lateUpdate", controller_env.__node, TICK) end
    if scout_env then call(scout_env, "lateUpdate", scout_env.__node, TICK) end
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
  -- A SIDE BOOSTER branch (builder v1.2 radial mounts): a radial decoupler
  -- on the tank's flank (disc rolled to face outward) + a booster engine on
  -- its outer face. Burns from ignition; SPACE kicks the whole branch away
  -- laterally BEFORE any axial stage fires.
  { uid = 6, id = "radialDec", prefab = "PartDecoupler", x = 1.02, y = 2.1, z = 0, yaw = 0,
    roll = -1.5708, h = 0.25, mass = 0.12, cost = 70, kind = "structural",
    thrust = 0, burn = 0, fuel = 0, decouple = 1, legs = 0, radial = 1, parent = 2, att = "radial" },
  { uid = 7, id = "engineS", prefab = "PartEngineS", x = 2.0, y = 2.1, z = 0, yaw = 0,
    h = 1.3, mass = 0.8, cost = 150, kind = "engine", thrust = 40, burn = 0.7,
    fuel = 0, decouple = 0, legs = 0, parent = 6 },
  -- LANDING LEGS: the first peripheral device — G toggles them in flight.
  { uid = 8, id = "legs", prefab = "PartLegs", x = 0, y = 0.65, z = 0.8, yaw = 0,
    h = 0.7, mass = 0.3, cost = 90, kind = "structural", thrust = 0, burn = 0,
    fuel = 0, decouple = 0, legs = 1 },
  -- A PARACHUTE on the nose: armed in staging, deploys to drag in atmosphere.
  { uid = 9, id = "chute", prefab = "PartChute", x = 0, y = 3.7, z = 0, yaw = 0,
    h = 0.61, mass = 0.1, cost = 80, kind = "canvas", thrust = 0, burn = 0,
    fuel = 0, decouple = 0, legs = 0, chute = 1 },
} }
save_store["shipyard.launch"] = 1
save_store["shipyard.pilot"] = 1

local astro = make_node("Astronaut", planet.x, planet.y + planet.radius + 6, planet.z)
local scout_node = make_node("Ship", planet.x, planet.y + planet.radius + 4, planet.z)
for _, nm in ipairs({ "Ship HUD Text", "Vessel HUD", "Stage List", "Navball", "Speed Tape",
                      "Alt Tape", "Speed Readout", "Alt Readout", "Heading Readout" }) do
  local n = make_node(nm, 0, 0, 0)
  n.components["UiElement"] = { visible = false }
  n.text = ""
end

-- A DECOY vessel: a second, unpiloted craft registered BEFORE the real one,
-- so findScript("vessel_controller") returns the WRONG instance — every
-- consumer that needs THE piloted vessel must scan findScripts (the
-- multi-craft world: landed ships, satellites, dropped stages).
local decoy_node = make_node("Vessel (parked)", planet.x + 40, planet.y + planet.radius, planet.z)
register_script("vessel_controller", { __node = decoy_node, __kind = "vessel_controller",
                                       piloting = false })

local sp_node = make_node("Vessel Spawner", 0, 0, 0)
spawner_env = load_script("solar/scripts/vessel_spawner.lua", "vessel_spawner", sp_node)
call(spawner_env, "start", sp_node)

-- The REAL scout (debug ship / map owner): it stashes itself on tick 1 and
-- its lateUpdate drives the 3D map for whatever craft is being flown.
scout_env = load_script("solar/scripts/ship_controller.lua", "ship_controller", scout_node)
call(scout_env, "start", scout_node)

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

-- The spawner honors the blueprint's full local rotation (v1.2 builds
-- sideways parts): the radial decoupler's disc must arrive rolled outward.
local rdec
for _, n in ipairs(nodes) do
  if n.name == "PartDecoupler" and math.abs((n.x or 0) - 1.02) < 0.1 then rdec = n end
end
check(rdec ~= nil and math.abs((rdec.roll or 0) + 1.5708) < 0.01,
  "radial decoupler spawns with its blueprint roll (sideways disc)")

-- Release, throttle: thrust + flames must fire.
press("space")
step(2)
check(asm and asm.anchored == false, "SPACE released the launch clamps")
API.KEYS.down["shift"] = true
step(30)
API.KEYS.down["shift"] = false
check(force_calls > 0, "throttle produced engine forceAt calls")
-- STAGE GATING: the bottom stack engine AND the attached side booster burn
-- (2 × ~30 ticks ≈ 60 calls); the upper-stage engine stays cold (3 burning
-- would be ~90).
check(force_calls >= 50 and force_calls <= 70,
  string.format("core + booster burn, upper stays cold (forceAt ×%d over 30 ticks)", force_calls))
check(torque_calls > 0, "attitude loop produced torque calls")
check(controller_env.throttle > 0, "throttle climbed under SHIFT")
local flame_lo, flame_hi, flame_boost
for _, n in ipairs(nodes) do
  if n.name == "Engine Flame" and n.parent then
    if (n.parent.x or 0) > 1.0 then flame_boost = n
    elseif n.parent.y < 1.5 then flame_lo = n
    else flame_hi = n end
  end
end
check(flame_lo ~= nil and flame_hi ~= nil and flame_boost ~= nil,
  "every engine prefab spawned a flame child")
check(flame_lo and flame_lo.particles_state.playing, "bottom-stage plume plays under throttle")
check(flame_lo and flame_lo.components["PointLight"].intensity > 0, "plume light follows throttle")
check(flame_hi and not flame_hi.particles_state.playing, "upper-stage plume stays COLD before staging")
check(flame_boost and flame_boost.particles_state.playing, "side-booster plume burns from ignition")

-- The flight stage list: separation EVENTS in firing order, next marked ▶ —
-- the booster ring fires first by default.
local stagen = find_node("Stage List")
check(stagen.components["UiElement"].visible ~= false and stagen.text:find("▶") ~= nil,
  "stage list paints in flight with the next event marked")
check(stagen.text:find("BOOSTER RING") ~= nil,
  "booster ring is the next event (got: " .. tostring(stagen.text) .. ")")

-- PERIPHERALS: G folds the MULTI-JOINTED landing legs up (both pivots roll
-- toward the stowed pose), G again folds them back down to the deployed pose.
local leg_node, hip_pivot, knee_pivot
for _, n in ipairs(nodes) do
  if n.name == "PartLegs" then leg_node = n end
  if n.name == "LegUpperPivot" then hip_pivot = n end
  if n.name == "LegKneePivot" then knee_pivot = n end
end
check(leg_node ~= nil, "legs part spawned")
check(hip_pivot ~= nil and knee_pivot ~= nil, "leg spawned its hip + knee pivots")
step(2) -- let the device apply the initial (deployed) pose
local dep_hip, dep_knee = hip_pivot.roll or 0.0, knee_pivot.roll or 0.0
press("g")
step(90)
check(controller_env.gear_deployed == false, "G retracts the gear")
check(math.abs((hip_pivot.roll or 0.0) - dep_hip) > 1.0
    and math.abs((knee_pivot.roll or 0.0) - dep_knee) > 1.0,
  string.format("both joints fold when retracted (hip %.2f, knee %.2f)",
    hip_pivot.roll or 0, knee_pivot.roll or 0))
press("g")
step(90)
check(controller_env.gear_deployed == true, "G redeploys the gear")
check(math.abs((hip_pivot.roll or 0.0) - dep_hip) < 0.05
    and math.abs((knee_pivot.roll or 0.0) - dep_knee) < 0.05,
  "both joints fold back to the deployed pose")

-- SPACE #2: side boosters kick away FIRST — the whole radial branch (its
-- decoupler + engine) leaves as one lateral group.
press("space")
step(2)
check(#split_calls == 1 and split_calls[1] == 2,
  string.format("boosters away as one 2-part branch (splits: %d)", #split_calls))
force_calls = 0
step(30)
check(force_calls >= 20 and force_calls <= 35,
  string.format("only the core engine burns after booster separation (forceAt ×%d)", force_calls))
check(flame_boost and not flame_boost.particles_state.playing,
  "booster plume dies with the separation")

-- SPACE #3: the AXIAL decoupler path (split stubbed; must not error).
press("space")
step(2)
check(#split_calls == 2, "axial stage fires after the boosters are gone")

-- SPACE #4: PARACHUTES — the last staging event. It doesn't split; it opens
-- the canopy, which then drags against velocity in the atmosphere. Put the
-- ship in a descent and check the drag force opposes the fall.
check(controller_env.chutes_deployed == false, "chutes start packed")
asm.vel = { x = 0, y = -40, z = 0 } -- falling toward the planet (which is +X)
API.LAST_FORCE = nil
press("space")
step(2)
check(controller_env.chutes_deployed == true, "SPACE deploys the parachutes")
step(30) -- canopy fills, drag builds
check(API.LAST_FORCE ~= nil and API.LAST_FORCE.y > 0,
  string.format("chute drag opposes the fall (Fy=%s)",
    API.LAST_FORCE and string.format("%.1f", API.LAST_FORCE.y) or "nil"))
check(hudn.text:find("CHUTES OPEN") ~= nil, "HUD shows the open chutes")
asm.vel = { x = 0, y = 0, z = 0 } -- settle before the EVA checks

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

-- ── THE MAP flies the piloted vessel (round 11) ─────────────────────────────
-- Put the piloted vessel "in flight" with a real orbital velocity (the engine
-- mirror feeds node.vx on compound roots; modeled by hand here), open the map
-- with M, and demand the SHIP'S conic is drawn + the info panel reads the
-- VESSEL as its subject. The subject swap must survive the scout being
-- stashed far outside every SOI.
check(scout_env.stashed == true, "scout stashed itself (debug tool)")
local svx, svy, svz = world_of(scout_node)
check(svy > 1.0e6, string.format("scout parked in deep space (y=%.0f)", svy))

asm.vel = { x = 47, y = 0, z = 0 }   -- ~circular at pad radius: sqrt(mu/r)
asm.root.vx, asm.root.vy, asm.root.vz = 47, 0, 0
asm.root.grounded = false
step(9)

press("m")
step(9) -- past the 10 Hz map-HUD throttle

-- …and stood down while the map owns the screen.
check(stagen.components["UiElement"].visible == false, "stage list hides under the map")

check(scout_env.map_view == true, "M opens the map while flying a built vessel")
local conic_drawn = false
for _, l in ipairs(tick_lines) do
  if math.abs(l.r - 0.35) < 0.01 and math.abs(l.g - 0.85) < 0.01 and math.abs(l.b - 1.0) < 0.01 then
    conic_drawn = true
    break
  end
end
check(conic_drawn, string.format(
  "map draws the piloted vessel's orbit conic (%d lines drawn this tick)", #tick_lines))
check(hudn.text:find("MAP") ~= nil and hudn.text:find("SHIP ORBIT") ~= nil,
  "map info panel reads the vessel's orbit (got: " .. tostring(hudn.text) .. ")")
press("m")
step(9) -- the vessel HUD repaints on its own 10 Hz clock
check(scout_env.map_view == false, "M closes the map again")
check(hudn.text:find("THR") ~= nil, "vessel HUD repaints after closing the map")

-- ── DESTRUCTION (round 11): per-part impacts → damage → breakup ─────────────
-- The engine attributes every contact's impulse to the part that took it
-- (assembly.impacts). Inject a survivable hit on the tank (smoulders + HUD
-- damage line), then a killing blow (explosion + the part shears off), then
-- kill the POD (flight over, pilot thrown clear).
local tank_node, pod_node
for _, n in ipairs(nodes) do
  if n.parent == asm.root then
    if n.name == "PartTankS" then tank_node = n end
    if n.name == "PartPod" then pod_node = n end
  end
end
check(tank_node ~= nil and pod_node ~= nil, "tank + pod children located")

-- Damage ARMS 2.5s after clamp release. The crash metric is IMPACT SPEED (m/s)
-- now (assembly.impacts .speed), not a mass-scaled impulse — inject `speed`. A
-- hard-but-survivable knock on the tank (fragile tolerance 3.5): speed 2.0 →
-- ~40% damage — enough to smoulder, not enough to break.
step(160)
local n_fx = #API.EFFECTS
API.IMPACTS = { { part = tank_node.id, impulse = 0.0, speed = 2.0,
                  x = tank_node.x, y = tank_node.y, z = tank_node.z } }
step(9)
check(#API.EFFECTS > n_fx and API.EFFECTS[n_fx + 1].name == "Smoke",
  "a hard-but-survivable hit smoulders (Smoke effect)")
check(hudn.text:find("DAMAGE") ~= nil,
  "HUD reports the damaged part (got: " .. tostring(hudn.text) .. ")")

-- A killing blow on the tank (>= tol 3.5, < shatter 13): it explodes, shears off
-- as its own single-part wreck, and — a fuel tank against the ground — craters.
-- (The tank's blast concusses neighbours but doesn't destroy them here, so it's
-- still exactly one split.)
local n_split = #split_calls
local n_crater = #API.CRATERS
API.IMPACTS = { { part = tank_node.id, impulse = 0.0, speed = 8.0,
                  x = tank_node.x, y = tank_node.y, z = tank_node.z } }
step(2)
local exploded = false
for _, e in ipairs(API.EFFECTS) do
  if e.name == "Explosion" then exploded = true end
end
check(exploded, "a full-strength hit explodes the part")
check(#split_calls == n_split + 1 and split_calls[#split_calls] == 1,
  "the broken part shears off as its own wreck")
check(#API.CRATERS > n_crater, "a tank blast against the ground digs a crater")

-- A CATASTROPHIC strike (>= shatter speed 20) fails the WHOLE airframe: every
-- surviving part bursts loose as wreckage (many splits, not one), a big crater,
-- and the pilot is thrown clear. A plain pod-break never splits anything, so any
-- split increase here proves the shatter path fired.
local n_split2 = #split_calls
local n_crater2 = #API.CRATERS
API.IMPACTS = { { part = pod_node.id, impulse = 0.0, speed = 26.0,
                  x = pod_node.x, y = pod_node.y, z = pod_node.z } }
step(2)
check(#split_calls - n_split2 >= 2, string.format(
  "a catastrophic hit shatters the ship into pieces (splits: %d)", #split_calls - n_split2))
check(#API.CRATERS > n_crater2, "the breakup leaves a crater")
check(controller_env.piloting == false, "the breakup ends the flight")
check(astro.visible == true, "the pilot is thrown clear (astronaut visible)")

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
