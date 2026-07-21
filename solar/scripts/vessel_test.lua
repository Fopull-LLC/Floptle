-- VESSEL FLIGHT TEST v0 — the first thing that flies a BUILT ship. Deliberately
-- raw: it exists to feel the compound physics (honest thrust points, torque,
-- staging splits) before the real blueprint flight controller lands. Attached
-- to the Vessel prefab; reads the same self-contained blueprint the spawner
-- used, so it knows where every engine sits and what it pushes.
--
--   V (near the vessel)   take / release control
--   SHIFT / CTRL          throttle up / down     X cut     Z full
--   W/S  A/D  Q/E         pitch / yaw / roll (rate-damped)
--   SPACE                 fire the next decoupler stage (real split!)
--
-- Thrust is applied AT each engine's blueprint offset — build something
-- lopsided and it will pirouette exactly like it should.

defaults = {
  torque = 60.0,       -- command torque strength per axis
  rate = 1.2,          -- target turn rate at full stick, rad/s
}

local bp = nil
local engines = {}     -- { {x,y,z, thrust} } vessel-local
local decouplers = {}  -- { {uid, y} } sorted bottom-up, fired in order
local controlled = false
local throttle = 0.0

local function load_bp()
  bp = save.get("shipyard.blueprint")
  engines, decouplers = {}, {}
  if not bp or not bp.parts then return end
  for _, d in pairs(bp.parts) do
    if (d.thrust or 0) > 0 then
      engines[#engines + 1] = { x = d.x, y = d.y, z = d.z, thrust = d.thrust }
    end
    if (d.decouple or 0) == 1 then
      decouplers[#decouplers + 1] = { uid = d.uid, y = d.y }
    end
  end
  table.sort(decouplers, function(a, b) return a.y < b.y end)
end

-- Vessel-frame basis from the node's YXZ euler (matches the engine).
local function basis(node)
  local cy, sy = math.cos(node.yaw), math.sin(node.yaw)
  local cx, sx = math.cos(node.pitch), math.sin(node.pitch)
  local cz, sz = math.cos(node.roll), math.sin(node.roll)
  -- R = Ry * Rx * Rz, columns = rotated axes.
  local rx = vec3(cy * cz + sy * sx * sz, cx * sz, -sy * cz + cy * sx * sz)
  local up = vec3(-cy * sz + sy * sx * cz, cx * cz, sy * sz + cy * sx * cz)
  local rz = vec3(sy * cx, -sx, cy * cx)
  return rx, up, rz
end

function start(node)
  load_bp()
end

function fixedUpdate(node, dt)
  local info = assembly.info(node)
  if not info then return end

  -- Take / release control near the vessel.
  if input.pressed("v") then
    controlled = not controlled
    if controlled and not bp then load_bp() end
    log(controlled and "vessel: flight test control ON (v0)" or "vessel: control off")
  end
  if not controlled then return end

  -- Throttle.
  if input.key("shift") then throttle = math.min(1.0, throttle + 0.8 * dt) end
  if input.key("ctrl") then throttle = math.max(0.0, throttle - 0.8 * dt) end
  if input.pressed("x") then throttle = 0.0 end
  if input.pressed("z") then throttle = 1.0 end

  local rx, up, rz = basis(node)

  -- Thrust at every live engine's blueprint offset (honest CoT: an off-axis
  -- engine torques the stack through the physics, not through code).
  if throttle > 0 then
    for _, e in ipairs(engines) do
      local px = node.x + rx.x * e.x + up.x * e.y + rz.x * e.z
      local py = node.y + rx.y * e.x + up.y * e.y + rz.y * e.z
      local pz = node.z + rx.z * e.x + up.z * e.y + rz.z * e.z
      local f = e.thrust * throttle
      assembly.forceAt(node, vec3(up.x * f, up.y * f, up.z * f), vec3(px, py, pz))
    end
  end

  -- Rate-damped attitude: command a turn rate on each vessel axis, torque
  -- toward it, damp everything else (a crude SAS).
  local cmd_p = (input.key("s") and 1 or 0) - (input.key("w") and 1 or 0)
  local cmd_y = (input.key("a") and 1 or 0) - (input.key("d") and 1 or 0)
  local cmd_r = (input.key("e") and 1 or 0) - (input.key("q") and 1 or 0)
  local w = info.angVel
  local want = {
    x = (rx.x * cmd_p + up.x * cmd_y + rz.x * cmd_r) * params.rate,
    y = (rx.y * cmd_p + up.y * cmd_y + rz.y * cmd_r) * params.rate,
    z = (rx.z * cmd_p + up.z * cmd_y + rz.z * cmd_r) * params.rate,
  }
  assembly.torque(node, vec3(
    (want.x - w.x) * params.torque,
    (want.y - w.y) * params.torque,
    (want.z - w.z) * params.torque
  ))

  -- SPACE: fire the lowest remaining decoupler — every part child at or
  -- below it (VESSEL-LOCAL height, straight off the blueprint) detaches as a
  -- live stage, with a separation shove.
  if input.pressed("space") and #decouplers > 0 then
    local dec = table.remove(decouplers, 1)
    local parts_nodes = {}
    for _, child in ipairs(node:children()) do
      if child.y <= dec.y + 0.01 then parts_nodes[#parts_nodes + 1] = child end
    end
    if #parts_nodes > 0 then
      assembly.split(node, parts_nodes, function(stage)
        local si = assembly.info(stage)
        if si then
          assembly.impulseAt(stage, vec3(up.x * -3, up.y * -3, up.z * -3), si.com)
        end
        log("stage away: " .. #parts_nodes .. " parts")
      end)
    end
  end
end
