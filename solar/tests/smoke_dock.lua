-- SMOKE HARNESS: docking. Runs the real vessel_controller.lua — TWO live
-- instances of it — against a stubbed engine and drives the whole mission
-- shape offline: a station in orbit, a lander closing on its port, soft
-- capture, the blueprint absorption, then an UNDOCK that has to hand the
-- departing half back as a flyable craft with its own blueprint and fuel.
--
--   luajit solar/tests/smoke_dock.lua
--
-- Fidelity notes (the bugs this stub is shaped to catch):
--  * `assembly.merge` re-parents the absorbed craft's part nodes under the
--    surviving root with their WORLD pose kept, and retires the old root. The
--    controller reads the merged geometry straight off those nodes, so a stub
--    that "merges" without re-expressing coordinates would hide a real bug.
--  * `assembly.split(…, prefab)` roots the detached half at a fresh Vessel
--    (scripts and all) — here that means loading a THIRD controller env and
--    running its `start`, exactly like the engine does a frame later.
--  * Ports face outward along the part's own +Y, signed by which face is free.
--    A port whose free face is misread never captures, or captures backwards.

local T = 0.0
local TICK = 1 / 60

-- ── node registry (parent-LOCAL coordinates, like the engine) ───────────────
local nodes, next_id = {}, 1

local function make_node(name, x, y, z)
  local n = {
    __id = next_id, id = next_id, name = name, valid = true, visible = true,
    x = x or 0, y = y or 0, z = z or 0,
    pitch = 0, roll = 0, yaw = 0, vx = 0, vy = 0, vz = 0,
    scale_x = 1, scale_y = 1, scale_z = 1,
    text = "", components = {}, kids = {}, parent = nil, shader_params = {},
  }
  n.children = function(self)
    local out = {}
    for _, k in ipairs(self.kids) do out[#out + 1] = k end
    return out
  end
  n.getcomponent = function(self, kind) return self.components[kind] end
  n.setShaderParam = function(self, k, a) self.shader_params[k] = a end
  n.particles = function() return nil end
  next_id = next_id + 1
  nodes[#nodes + 1] = n
  return n
end

local function attach(child, parent)
  child.parent = parent
  parent.kids[#parent.kids + 1] = child
end

-- World position of a node (one level of nesting is all this scene needs).
local function world_of(n)
  if not n.parent then return n.x, n.y, n.z end
  local px, py, pz = world_of(n.parent)
  return px + n.x, py + n.y, pz + n.z
end

-- ── assemblies ──────────────────────────────────────────────────────────────
-- One compound per root, keyed by node. Enough physics to be honest about what
-- the controller can observe: mass, origin, velocity and the part list.
local asms = {}

local function add_asm(root, mass)
  local parts = {}
  for _, k in ipairs(root.kids) do parts[#parts + 1] = k.__id end
  asms[root] = { root = root, parts = parts, mass = mass or 6.0,
                 vel = { x = 0, y = 0, z = 0 }, anchored = false }
  return asms[root]
end

-- ── engine API ──────────────────────────────────────────────────────────────
local API = {}
local logs, effects = {}, {}

function API.log(msg) logs[#logs + 1] = tostring(msg) end
function API.vec3(x, y, z) return { x = x, y = y, z = z } end
function API.spawnEffect(name, x, y, z) effects[#effects + 1] = name end
API.draw = { ring = function() end, line = function() end, sphere = function() end,
             box = function() end }
API.terrain = { warm = function() end, query = function() return 50.0 end,
                dig = function() end, paint = function() end }
API.physics = { pause = function() end, isPaused = function() return false end }
local KEYS = {}
API.input = { key = function(k) return KEYS[k] == true end,
              pressed = function(k) return false end,
              mouseX = function() return 0 end, mouseY = function() return 0 end }
local function sound_handle()
  return { stop = function() end, setVolume = function() end, setPitch = function() end,
           isPlaying = function() return false end }
end
API.audio = { play = function() return sound_handle() end,
              track = function() return { setVolume = function() end } end }

-- A single sun-like body far away: enough for `space.dominant` and the solar
-- panel maths to run without dominating the test.
local SUN = { name = "Sun", x = 0, y = -1e6, z = 0, radius = 1e5, mu = 1e12, soi = 1e9 }
API.space = {
  bodies = function() return { SUN } end,
  body = function(name) return (name == "Sun") and SUN or nil end,
  dominant = function() return "Sun" end,
  elements = function() return nil end,
  warp = function() return 1.0 end,
}

local store = {}
API.save = {
  get = function(k) return store[k] end,
  set = function(k, v) store[k] = v end,
  flush = function() end,
}

-- ── script envs ─────────────────────────────────────────────────────────────
local script_envs = {}
local pending_start = {}

local function handle_of(env)
  return setmetatable({ node = env.__node }, {
    __index = env,
    __newindex = function(_, k, v) env[k] = v end,
  })
end

function API.find(name)
  for _, n in ipairs(nodes) do
    if n.name == name and n.valid then return n end
  end
  return nil
end
function API.findScript(kind)
  local l = script_envs[kind]
  return (l and l[1]) and handle_of(l[1]) or nil
end
function API.findScripts(kind)
  local out = {}
  for _, env in ipairs(script_envs[kind] or {}) do
    if env.__node.valid then out[#out + 1] = handle_of(env) end
  end
  return out
end

local function load_script(path, kind, node)
  local env = setmetatable({}, { __index = function(_, k)
    if k == "time" then return T end
    return API[k] or _G[k]
  end })
  env.__node, env.__kind = node, kind
  local chunk = assert(loadfile(path))
  setfenv(chunk, env)
  chunk()
  env.params = {}
  for k, v in pairs(env.defaults or {}) do env.params[k] = v end
  script_envs[kind] = script_envs[kind] or {}
  table.insert(script_envs[kind], env)
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

-- ── assembly stub ───────────────────────────────────────────────────────────
local merges, splits = 0, 0
local impulses, forces = {}, {}

API.assembly = {
  info = function(node)
    local a = asms[node]
    if not a then return nil end
    local x, y, z = world_of(node)
    return { mass = a.mass, com = { x = x, y = y, z = z },
             origin = { x = x, y = y, z = z },
             vel = { x = a.vel.x, y = a.vel.y, z = a.vel.z },
             angVel = { x = 0, y = 0, z = 0 },
             grounded = false, anchored = a.anchored, parts = a.parts }
  end,
  rebuild = function(node) add_asm(node) end,
  setAnchored = function(node, on) if asms[node] then asms[node].anchored = on end end,
  keepLive = function() end,
  syncColliders = function() end,
  teleport = function(node, pos) node.x, node.y, node.z = pos.x, pos.y, pos.z end,
  forceAt = function(node, f, at)
    forces[#forces + 1] = { node = node, x = f.x, y = f.y, z = f.z,
                            ax = at.x, ay = at.y, az = at.z, at = true }
  end,
  force = function(node, f)
    forces[#forces + 1] = { node = node, x = f.x, y = f.y, z = f.z }
  end,
  torque = function() end,
  impulseAt = function(node, imp, at)
    impulses[#impulses + 1] = { node = node, x = imp.x, y = imp.y, z = imp.z }
  end,
  impacts = function() return {} end,

  -- MERGE, as the engine does it: every node hanging off `other`'s root
  -- re-parents under `node`'s root keeping its WORLD pose (so the absorbed
  -- coordinates the controller reads back are genuinely in the new frame), the
  -- two part lists join, and `other`'s root is retired.
  merge = function(node, other)
    local a, b = asms[node], asms[other]
    if not a or not b then return end
    merges = merges + 1
    local ax, ay, az = world_of(node)
    for _, k in ipairs(other:children()) do
      local wx, wy, wz = world_of(k)
      k.x, k.y, k.z = wx - ax, wy - ay, wz - az
      attach(k, node)
      a.parts[#a.parts + 1] = k.__id
    end
    other.kids = {}
    a.mass = a.mass + b.mass
    asms[other] = nil
    other.valid = false
  end,

  -- SPLIT: the departing parts leave the root, and with a prefab name the new
  -- half is rooted at a live Vessel — a fresh controller env whose `start` runs
  -- on the next pass, exactly like the engine's spawn.
  split = function(node, parts, cb, prefab)
    splits = splits + 1
    local a = asms[node]
    -- The engine roots the detached half at ITS OWN centre of mass (that's what
    -- `local_origin = 0` on the detached compound means), NOT at the parent's
    -- origin. Anything that reasons about where the new craft is — the handoff
    -- blueprint's re-basing, the separation kick's direction — depends on that,
    -- so the stub has to place it the same way.
    local cx, cy, cz = 0, 0, 0
    for _, pn in ipairs(parts) do
      local wx, wy, wz = world_of(pn)
      cx, cy, cz = cx + wx, cy + wy, cz + wz
    end
    local inv = 1.0 / math.max(1, #parts)
    local root = make_node(prefab and "Vessel" or "Debris", cx * inv, cy * inv, cz * inv)
    for _, pn in ipairs(parts) do
      for i, k in ipairs(node.kids) do
        if k == pn then table.remove(node.kids, i) break end
      end
      local wx, wy, wz = world_of(pn)
      pn.x, pn.y, pn.z = wx - root.x, wy - root.y, wz - root.z
      attach(pn, root)
    end
    if a then
      local keep = {}
      for _, id in ipairs(a.parts) do
        local gone = false
        for _, pn in ipairs(parts) do
          if pn.__id == id then gone = true break end
        end
        if not gone then keep[#keep + 1] = id end
      end
      a.parts = keep
    end
    add_asm(root, 2.0)
    if cb then cb(root) end
    if prefab then
      local env = load_script("solar/scripts/vessel_controller.lua",
        "vessel_controller", root)
      pending_start[#pending_start + 1] = env
    end
    return root
  end,
}

-- ── the craft ───────────────────────────────────────────────────────────────
-- STATION: a crewed pod with a docking port on top (its free face points up).
local STATION = { parts = {
  { uid = 1, id = "pod", prefab = "PartPod", label = "Pod Mk1", x = 0, y = 0.40, z = 0,
    yaw = 0, pitch = 0, roll = 0, parent = 0, att = "", h = 0.80, mass = 1.2,
    kind = "crewed", power = 1, ec = 200 },
  { uid = 2, id = "dockPort", prefab = "PartDockPort", label = "Docking Port",
    x = 0, y = 0.94, z = 0, yaw = 0, pitch = 0, roll = 0, parent = 1, att = "",
    h = 0.28, mass = 0.18, kind = "structural", dock = 1 },
} }

-- LANDER: a docking port on the BOTTOM (free face points down), a tank, a pod.
local LANDER = { parts = {
  { uid = 1, id = "dockPort", prefab = "PartDockPort", label = "Docking Port",
    x = 0, y = 0.14, z = 0, yaw = 0, pitch = 0, roll = 0, parent = 0, att = "",
    h = 0.28, mass = 0.18, kind = "structural", dock = 1 },
  { uid = 2, id = "tankS", prefab = "PartTankS", label = "FT-S Tank", x = 0, y = 0.78,
    z = 0, yaw = 0, pitch = 0, roll = 0, parent = 1, att = "", h = 1.00, mass = 1.5,
    kind = "tank", fuel = 60 },
  { uid = 3, id = "pod", prefab = "PartPod", label = "Pod Mk1", x = 0, y = 1.68, z = 0,
    yaw = 0, pitch = 0, roll = 0, parent = 2, att = "", h = 0.80, mass = 1.2,
    kind = "crewed" },
} }

local function build(name, bp, x, y, z)
  local root = make_node(name, x, y, z)
  for _, d in pairs(bp.parts) do
    local part = make_node(d.prefab, d.x, d.y, d.z)
    attach(part, root)
    if d.dock == 1 then
      local lamp = make_node("Dock Light", 0, 0, 0)
      lamp.components.PointLight = { intensity = 0, r = 1, g = 1, b = 1 }
      attach(lamp, part)
    end
  end
  add_asm(root, 6.0)
  return root
end

make_node("Astronaut", 0, 0, 0)

local station_node = build("Station", STATION, 0, 0, 0)
local lander_node = build("Lander", LANDER, 0, 2.60, 0)

-- The station is the piloted craft; the lander is the target closing on it.
-- The blueprint is read in `start`, so it's swapped in right before each one.
local station = load_script("solar/scripts/vessel_controller.lua", "vessel_controller",
  station_node)
local lander = load_script("solar/scripts/vessel_controller.lua", "vessel_controller",
  lander_node)

store["shipyard.blueprint"] = STATION
call(station, "start", station_node)
store["shipyard.blueprint"] = LANDER
call(lander, "start", lander_node)
store["shipyard.blueprint"] = nil
station.piloting = true

-- ── loop ────────────────────────────────────────────────────────────────────
local function tick()
  T = T + TICK
  -- EVERY live controller ticks, including craft that appeared mid-run: an
  -- undocked module is a first-class vessel from its first frame, and a
  -- harness that only ticked the originals would quietly freeze it.
  for _, env in ipairs(script_envs.vessel_controller or {}) do
    if env.__node.valid then call(env, "fixedUpdate", env.__node, TICK) end
  end
  local starts = pending_start
  pending_start = {}
  for _, env in ipairs(starts) do
    call(env, "start", env.__node)
    call(env, "fixedUpdate", env.__node, TICK)
  end
  for _, env in ipairs(script_envs.vessel_controller or {}) do
    if env.__node.valid then call(env, "lateUpdate", env.__node, TICK) end
  end
end

-- ── checks ──────────────────────────────────────────────────────────────────
local checks = {}
local function check(desc, ok, detail)
  checks[#checks + 1] = { desc = desc, ok = ok and true or false, detail = detail }
end

-- 1. Both craft find their ports, with the right free faces.
check("station finds its port", #station.dock.ports == 1,
  "ports=" .. #station.dock.ports)
check("lander finds its port", #lander.dock.ports == 1, "ports=" .. #lander.dock.ports)
local sp, lp = station.dock.ports[1], lander.dock.ports[1]
check("station port faces UP (its pod is below)", sp and sp.ay > 0.99,
  sp and string.format("%.2f", sp.ay) or "nil")
check("lander port faces DOWN (its stack is above)", lp and lp.ay < -0.99,
  lp and string.format("%.2f", lp.ay) or "nil")
check("neither port reads as buried", sp and lp and not sp.buried and not lp.buried)

-- 2. The approach readout comes up while the lander is still outside capture
-- range — this is what the pilot flies on, so it has to be live and honest.
tick()
check("station sees the lander on approach", station.dock.target ~= nil)
if station.dock.target then
  local t = station.dock.target
  check("approach range is the real port gap (~1.80 m)",
    math.abs(t.range - 1.80) < 0.02, string.format("%.3f", t.range))
  check("approach reads squarely aligned", t.align > 0.99,
    string.format("%.3f", t.align))
  check("nothing latches at that range", station.dock.latched == 0)
end

-- 3. Fly it in. Capture on contact, absorption on the following tick.
for _ = 1, 30 do
  if lander_node.valid and lander_node.y > 1.30 then
    lander_node.y = math.max(1.30, lander_node.y - 0.06)
  end
  tick()
end
check("the assemblies merged exactly once", merges == 1, "merges=" .. merges)
check("the lander's root is retired", not lander_node.valid)
check("the station carries the whole stack now", #station_node.kids == 5,
  "kids=" .. #station_node.kids)
check("both ports read latched", station.dock.latched == 2,
  "latched=" .. tostring(station.dock.latched))
check("the lander's fuel came aboard", station.fuel > 59,
  string.format("%.1f", station.fuel))
local ports = station.dockPorts()
check("the station now lists two ports", #ports == 2, "#ports=" .. #ports)
check("both list entries are mated", ports[1] and ports[2] and ports[1].mate
  and ports[2].mate)

-- 4. The peripherals interface reports the merged craft.
local devs = station.peripherals()
check("the peripherals list is callable", type(devs) == "table")

-- 5. UNDOCK: the lander must come away as a LIVE craft with its own blueprint.
local before = #station_node.kids
-- Undocking is requested from the row the pilot happens to click. `ports[1]`
-- is the MODULE's own port (highest on the stack), so this also proves the cut
-- anchors on the seam rather than on whichever side was named.
check("undock reports success", station.undock(ports[1].uid) == true)
for _ = 1, 4 do tick() end
check("exactly one split fired", splits == 1, "splits=" .. splits)
check("the station kept only its own parts", #station_node.kids == before - 3,
  string.format("%d → %d", before, #station_node.kids))
check("the station's ports are free again", station.dock.latched == 0,
  "latched=" .. tostring(station.dock.latched))
local module_env
for _, env in ipairs(script_envs.vessel_controller or {}) do
  if env ~= station and env ~= lander and env.__node.valid then module_env = env end
end
check("the departed half woke up as a live vessel", module_env ~= nil)
if module_env then
  check("it flew off with the lander's 3 parts and no others",
    #module_env.__node.kids == 3, "kids=" .. #module_env.__node.kids)
  check("it kept the fuel it undocked with", module_env.fuel > 55 and
    module_env.fuel <= 60, string.format("%.1f", module_env.fuel or -1))
  check("its own port is free and pointing down",
    #module_env.dock.ports == 1 and module_env.dock.ports[1].ay < -0.99)
  check("it can be crewed (control transfer has somewhere to go)",
    module_env.takeControl() == true)
end
check("the station's fuel left with the lander", station.fuel < 5.0,
  string.format("%.1f", station.fuel))
-- The separation push must send the module AWAY. A mated port's own +Y points
-- into its partner, so an unsigned kick here would fire it back through the
-- ship it just undocked from.
local sep = impulses[#impulses]
check("the separation kick pushes the module clear (away, not back through us)",
  sep ~= nil and sep.y > 0, sep and string.format("%.2f, %.2f, %.2f", sep.x, sep.y, sep.z))

-- 6. RE-DOCK: the whole point of the mission shape. Wait out the re-capture
-- lockout (which exists so a fresh undock can't snap straight back on), then
-- fly the module home and latch again.
if module_env then
  local m_node = module_env.__node
  -- Fly it straight back onto the seam, closing off the LIVE approach readout
  -- rather than hand-computed coordinates.
  local function close_in(ticks)
    for _ = 1, ticks do
      local t = module_env.dock and module_env.dock.target
      if t and t.range > 0.30 then
        m_node.y = m_node.y - math.min(0.03, t.range * 0.5)
      end
      tick()
      if (module_env.dock.latched or 0) > 0 then return true end
    end
    return false
  end
  -- Nothing may latch while the re-capture lockout is running: that lockout is
  -- the only thing stopping a fresh undock from snapping back on before it can
  -- drift clear, and it has to hold even parked right on the seam.
  local early = close_in(90)
  check("the lockout refuses to re-latch a just-undocked module", not early,
    "range=" .. string.format("%.2f",
      (module_env.dock.target and module_env.dock.target.range) or -1))
  local relatched = close_in(300)
  -- The module is the craft being flown, so IT absorbs the station — the
  -- piloted half always wins the master election, which is what keeps the
  -- pilot's HUD, fuel pool and controls on the craft they're actually in.
  check("it docks again once the lockout expires", relatched,
    "latched=" .. tostring(module_env.dock.latched))
  check("the second capture merged again", merges == 2, "merges=" .. merges)
  check("the piloted half absorbed the other", not station_node.valid)
  check("the stack is whole again", #m_node.kids == 5, "kids=" .. #m_node.kids)
end

-- ── 7. ONE craft, built with a port PAIR as its seam ────────────────────────
-- This is the build people actually make: a mothership with a docking port on
-- top, a lander with a port on its bottom, stacked in the VAB and launched as a
-- single stack. Nothing ever "captured" that seam, so nothing set its latch —
-- if the flight side only trusts captures, the pair reads as two blocked ports
-- and the lander can never leave. Parked far away so the craft above ignore it.
local STACK = { parts = {
  { uid = 1, id = "pod", prefab = "PartPod", label = "Pod Mk1", x = 0, y = 0.40, z = 0,
    yaw = 0, pitch = 0, roll = 0, parent = 0, att = "", h = 0.80, mass = 1.2,
    kind = "crewed", power = 1, ec = 200 },
  { uid = 2, id = "dockPort", prefab = "PartDockPort", label = "Docking Port",
    x = 0, y = 0.94, z = 0, yaw = 0, pitch = 0, roll = 0, parent = 1, att = "",
    h = 0.28, mass = 0.18, kind = "structural", dock = 1 },
  { uid = 3, id = "dockPort", prefab = "PartDockPort", label = "Docking Port",
    x = 0, y = 1.22, z = 0, yaw = 0, pitch = 0, roll = 0, parent = 2, att = "",
    h = 0.28, mass = 0.18, kind = "structural", dock = 1 },
  { uid = 4, id = "tankS", prefab = "PartTankS", label = "FT-S Tank", x = 0, y = 1.86,
    z = 0, yaw = 0, pitch = 0, roll = 0, parent = 3, att = "", h = 1.00, mass = 1.5,
    kind = "tank", fuel = 60 },
  { uid = 5, id = "pod", prefab = "PartPod", label = "Pod Mk1", x = 0, y = 2.76, z = 0,
    yaw = 0, pitch = 0, roll = 0, parent = 4, att = "", h = 0.80, mass = 1.2,
    kind = "crewed" },
} }

local stack_node = build("Stack", STACK, 900, 0, 0)
local stack = load_script("solar/scripts/vessel_controller.lua", "vessel_controller",
  stack_node)
store["shipyard.blueprint"] = STACK
call(stack, "start", stack_node)
store["shipyard.blueprint"] = nil
tick()

check("a VAB-stacked port pair reads as a live seam, not two blocked ports",
  stack.dock.latched == 2, "latched=" .. tostring(stack.dock.latched))
local srows = stack.dockPorts()
check("both rows offer UNDOCK", #srows == 2 and srows[1].mate and srows[2].mate
  and not srows[1].buried and not srows[2].buried)
check("the whole stack is one craft until you undock it", #stack_node.kids == 5,
  "kids=" .. #stack_node.kids)

-- Undock from the LOWER row (the mothership's own port) — the seam must cut
-- between the two ports, leaving one port on each half so they can re-mate.
local msplits = splits
check("undocking the built-in seam works", stack.undock(srows[2].uid) == true)
for _ = 1, 4 do tick() end
check("it split once", splits == msplits + 1)
check("the mothership kept its pod AND its port", #stack_node.kids == 2,
  "kids=" .. #stack_node.kids)
check("its port is free again, ready to re-mate", #stack.dock.ports == 1
  and not stack.dock.ports[1].mate and not stack.dock.ports[1].buried)
local lander_env
for _, env in ipairs(script_envs.vessel_controller or {}) do
  if env.__node.valid and env.__node.x > 800 and env ~= stack then lander_env = env end
end
check("the lander came away alive", lander_env ~= nil)
if lander_env then
  check("it took port + tank + pod (3 parts)", #lander_env.__node.kids == 3,
    "kids=" .. #lander_env.__node.kids)
  check("it kept a port of its OWN to dock back with",
    #lander_env.dock.ports == 1 and not lander_env.dock.ports[1].mate
    and not lander_env.dock.ports[1].buried)
  check("it flew off with the tank's fuel", lander_env.fuel > 55,
    string.format("%.1f", lander_env.fuel or -1))
end

-- ── 8. RCS, the docking autopilot, side berths, and hull contact ────────────
-- A tug with four thruster blocks and a docking port on its FLANK: the build
-- the side-berth mission needs, and the one that breaks if a port's mating
-- face is read as "up" just because that's the part's own +Y.
local TUG = { parts = {
  { uid = 1, id = "pod", prefab = "PartPod", label = "Pod Mk1", x = 0, y = 0.40, z = 0,
    yaw = 0, pitch = 0, roll = 0, parent = 0, att = "", h = 0.80, rx = 0.5, rz = 0.5,
    mass = 1.2, kind = "crewed", power = 1, ec = 200 },
  { uid = 2, id = "tankS", prefab = "PartTankS", label = "FT-S Tank", x = 0, y = 1.30,
    z = 0, yaw = 0, pitch = 0, roll = 0, parent = 1, att = "", h = 1.00, rx = 0.5,
    rz = 0.5, mass = 1.5, kind = "tank", fuel = 60 },
  -- Flank port: radial_orient yaws it so its local +X faces −Z (yaw = π/2 puts
  -- +X on −Z), i.e. straight out of the hull.
  { uid = 3, id = "dockPort", prefab = "PartDockPort", label = "Docking Port",
    x = 0, y = 1.30, z = -1.03, yaw = math.pi * 0.5, pitch = 0, roll = 0,
    parent = 2, att = "radial", h = 0.28, rx = 0.53, rz = 0.53, mass = 0.18,
    kind = "structural", dock = 1 },
  { uid = 4, id = "rcs", prefab = "PartRCS", label = "RCS Block", x = 0.7, y = 1.30,
    z = 0, yaw = 0, pitch = 0, roll = 0, parent = 2, att = "radial", h = 0.34,
    rx = 0.2, rz = 0.2, mass = 0.08, kind = "structural", rcs = 1, rcs_thrust = 2.0 },
  { uid = 5, id = "rcs", prefab = "PartRCS", label = "RCS Block", x = -0.7, y = 1.30,
    z = 0, yaw = math.pi, pitch = 0, roll = 0, parent = 2, att = "radial", h = 0.34,
    rx = 0.2, rz = 0.2, mass = 0.08, kind = "structural", rcs = 1, rcs_thrust = 2.0 },
} }

local tug_node = build("Tug", TUG, -900, 0, 0)
local tug = load_script("solar/scripts/vessel_controller.lua", "vessel_controller",
  tug_node)
store["shipyard.blueprint"] = TUG
call(tug, "start", tug_node)
store["shipyard.blueprint"] = nil
tug.piloting = true
tick()

-- A FLANK-mounted port mates OUTWARD (its local +X), not along the part's +Y.
-- Reading the wrong axis is why a side berth used to point at the sky.
local tp = tug.dock.ports[1]
check("the tug finds its flank port", tp ~= nil)
if tp then
  check("a side berth faces OUT of the hull, not up",
    math.abs(tp.ay) < 0.01 and math.abs(tp.az) > 0.99,
    tp and string.format("(%.2f, %.2f, %.2f)", tp.ax, tp.ay, tp.az))
  check("and it isn't reported as buried", not tp.buried)
end

-- RCS: fitted, off at rollout, and it pools its blocks' thrust.
local devs = tug.peripherals()
local rcs_row
for _, d in ipairs(devs) do if d.id == "rcs" then rcs_row = d end end
check("RCS shows up in the peripherals console", rcs_row ~= nil)
if rcs_row then
  check("...as a 2-block bus", rcs_row.count == 2, "count=" .. rcs_row.count)
  check("...cold on the pad", not rcs_row.on)
  check("...labelled ON/OFF, not DEPLOYED/STOWED", rcs_row.verbOn == "ON")
end

-- Thrusters off: the stick does nothing at all.
forces = {}
KEYS.h = true
tick()
local pushed = false
for _, f in ipairs(forces) do
  if f.node == tug_node and not f.at then pushed = true end
end
check("a cold RCS bus ignores the stick", not pushed)

-- Thrusters on: I pushes along the NOSE (+Y here), and only while held.
tug.setPeripheral("rcs", true)
forces = {}
tick()
local fy, fx = 0, 0
for _, f in ipairs(forces) do
  if f.node == tug_node and not f.at then fy, fx = fy + f.y, fx + f.x end
end
check("RCS pushes along the nose on H (fore)", fy > 3.0 and math.abs(fx) < 0.01,
  string.format("fy=%.2f fx=%.2f", fy, fx))
check("it pooled both blocks (2 × 2 kN)", math.abs(tug.rcs.thrust - 4.0) < 1e-6,
  string.format("%.2f", tug.rcs.thrust))
check("and it reports firing", tug.rcs.firing)
local fuel_before = tug.fuel
for _ = 1, 30 do tick() end
check("firing burns propellant", tug.fuel < fuel_before,
  string.format("%.2f → %.2f", fuel_before, tug.fuel))
KEYS.h = nil
forces = {}
tick()
pushed = false
for _, f in ipairs(forces) do
  if f.node == tug_node and not f.at then pushed = true end
end
check("releasing the stick stops the thrusters", not pushed and not tug.rcs.firing)

-- HULL CONTACT: park a second craft overlapping the tug. Nothing should be
-- able to sit inside another ship — the interim bumper has to push them apart.
local blocker_node = build("Blocker", STATION, -899.4, 1.3, 0)
local blocker = load_script("solar/scripts/vessel_controller.lua",
  "vessel_controller", blocker_node)
store["shipyard.blueprint"] = STATION
call(blocker, "start", blocker_node)
store["shipyard.blueprint"] = nil
forces = {}
tick()
local bump_x, bumped = 0, false
for _, f in ipairs(forces) do
  if f.node == tug_node and f.at then
    bumped, bump_x = true, bump_x + f.x
  end
end
check("overlapping hulls push apart", bumped, "no contact force")
check("...and the push is AWAY from the other craft", bump_x < 0,
  string.format("%.1f", bump_x))
-- Docking ports are exempt: they have to touch to latch.
local spheres = tug.hullSpheres()
check("docking ports are exempt from hull contact", #spheres == 4,
  "#spheres=" .. #spheres)

-- ── report ──────────────────────────────────────────────────────────────────
local bad = {}
for _, c in ipairs(checks) do
  if not c.ok then
    bad[#bad + 1] = "  ✗ " .. c.desc .. (c.detail and ("   [" .. c.detail .. "]") or "")
  end
end
for _, f in ipairs(failures) do bad[#bad + 1] = "  ✗ SCRIPT ERROR " .. f end

if #bad == 0 then
  print(string.format("DOCKING SMOKE OK — %d checks passed", #checks))
  os.exit(0)
end
print("DOCKING SMOKE FAILURES:")
for _, b in ipairs(bad) do print(b) end
print("\n-- script logs --")
for _, l in ipairs(logs) do print("  " .. l) end
os.exit(1)
