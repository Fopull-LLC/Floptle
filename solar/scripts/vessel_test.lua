-- VESSEL PILOT v1 — flies a BUILT ship (the compound assembly the spawner
-- rebuilt from your blueprint). Attached to the Vessel prefab; reads the same
-- self-contained blueprint, so it knows where every engine sits and where the
-- pod is.
--
-- Launching from the builder puts you IN the pod from the first frame (no
-- astronaut standing around), with the vessel CLAMPED to the launchpad —
-- anchored physics, riding the planet — until you release:
--
--   SPACE                 first press releases the launch clamps;
--                         after that, fires the next decoupler stage
--   SHIFT / CTRL          throttle up / down     X cut     Z full
--   W/S  A/D  Q/E         pitch / yaw / roll (rate-damped)
--   F                     exit the pod (EVA) / board again at the pod
--
-- Thrust is applied AT each engine's blueprint offset — build something
-- lopsided and it will pirouette exactly like it should.

defaults = {
  torque = 60.0,       -- command torque strength per axis
  rate = 1.2,          -- target turn rate at full stick, rad/s
  board_range = 3.4,   -- how close the astronaut must be to the pod to board
}

-- Published (planet_walker defers, planet_camera follows the vessel).
piloting = false

local bp = nil
local engines = {}     -- { {x,y,z, thrust} } vessel-local
local decouplers = {}  -- { {uid, y} } sorted bottom-up, fired in order
local pod = { x = 0, y = 0.5, z = 0 } -- crew seat, vessel-local
local part_total = 0
local throttle = 0.0
local boarding = false -- launch handoff: board as soon as the astronaut exists
local astronaut = nil
local hud = nil
local hud_last = nil

local function load_bp()
  bp = save.get("shipyard.blueprint")
  engines, decouplers, part_total = {}, {}, 0
  if not bp or not bp.parts then return end
  local top = -math.huge
  for _, d in pairs(bp.parts) do
    part_total = part_total + 1
    if (d.thrust or 0) > 0 then
      engines[#engines + 1] = { x = d.x, y = d.y, z = d.z, thrust = d.thrust }
    end
    if (d.decouple or 0) == 1 then
      decouplers[#decouplers + 1] = { uid = d.uid, y = d.y }
    end
    if d.kind == "crewed" then pod = { x = d.x, y = d.y, z = d.z } end
    top = math.max(top, d.y)
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

local function pod_world(node)
  local rx, up, rz = basis(node)
  return node.x + rx.x * pod.x + up.x * pod.y + rz.x * pod.z,
         node.y + rx.y * pod.x + up.y * pod.y + rz.y * pod.z,
         node.z + rx.z * pod.x + up.z * pod.y + rz.z * pod.z
end

local function set_hud(text)
  if not hud then hud = find("Vessel HUD") end
  if hud and text ~= hud_last then
    hud.text = text
    hud_last = text
  end
end

function start(node)
  load_bp()
  -- Arriving from the builder's LAUNCH: you start in the pod, not beside it.
  if (save.get("shipyard.pilot") or 0) == 1 then
    save.set("shipyard.pilot", 0)
    boarding = true
  end
end

local runaway_logged = false

function fixedUpdate(node, dt)
  if not astronaut or not astronaut.valid then astronaut = find("Astronaut") end
  local info = assembly.info(node)

  -- Launch handoff: climb into the pod the moment the astronaut node exists.
  if boarding and astronaut then
    boarding = false
    piloting = true
    astronaut.visible = false
    log("vessel: you're in the pod — SHIFT to throttle, SPACE releases the clamps")
  end

  -- Runaway diagnostic: an UNCONTROLLED vessel has no business exceeding
  -- 50 u/s in its planet's frame — log everything once so reports have data.
  if info and not piloting and not runaway_logged then
    local v = info.vel
    local speed = math.sqrt(v.x * v.x + v.y * v.y + v.z * v.z)
    if speed > 50 then
      runaway_logged = true
      log(string.format(
        "VESSEL RUNAWAY: speed %.0f  pos (%.1f, %.1f, %.1f)  com (%.1f, %.1f, %.1f)  grounded=%s",
        speed, node.x, node.y, node.z, info.com.x, info.com.y, info.com.z,
        tostring(info.grounded)))
    end
  end

  local px, py, pz = pod_world(node)
  local rx, up, rz = basis(node)

  -- ---- board / exit (F, same verb as the scout ship) ----------------------
  if input.pressed("f") and astronaut then
    if piloting then
      piloting = false
      throttle = 0.0
      -- Step out beside the pod, inheriting the vessel's velocity.
      astronaut.x = px + rx.x * 2.2 + up.x * 0.4
      astronaut.y = py + rx.y * 2.2 + up.y * 0.4
      astronaut.z = pz + rx.z * 2.2 + up.z * 0.4
      if info then
        astronaut.vx, astronaut.vy, astronaut.vz = info.vel.x, info.vel.y, info.vel.z
      end
      astronaut.visible = true
      set_hud("")
    elseif distance(astronaut, vec3(px, py, pz)) <= params.board_range then
      piloting = true
      astronaut.visible = false
    end
  end

  -- The seated astronaut rides the pod (bodies don't push each other).
  if piloting and astronaut then
    astronaut.x, astronaut.y, astronaut.z = px, py, pz
    if info then
      astronaut.vx, astronaut.vy, astronaut.vz = info.vel.x, info.vel.y, info.vel.z
    else
      astronaut.vx, astronaut.vy, astronaut.vz = 0, 0, 0
    end
  end

  if not piloting then
    -- EVA: telegraph the hatch when the astronaut is close enough to board.
    if astronaut and astronaut.visible
      and distance(astronaut, vec3(px, py, pz)) <= params.board_range + 1.2 then
      draw.ring(px, py, pz, up.x, up.y, up.z, 0.75, 0.3, 0.95, 1.0, 0.9)
      set_hud("F — board")
    else
      set_hud("")
    end
    return
  end
  if not info then
    set_hud(part_total > 0 and "vessel assembling…" or "")
    return
  end

  -- ---- flight -------------------------------------------------------------
  if input.key("shift") then throttle = math.min(1.0, throttle + 0.8 * dt) end
  if input.key("ctrl") then throttle = math.max(0.0, throttle - 0.8 * dt) end
  if input.pressed("x") then throttle = 0.0 end
  if input.pressed("z") then throttle = 1.0 end

  -- Thrust at every live engine's blueprint offset (honest CoT: an off-axis
  -- engine torques the stack through the physics, not through code).
  if throttle > 0 and not info.anchored then
    for _, e in ipairs(engines) do
      local ex = node.x + rx.x * e.x + up.x * e.y + rz.x * e.z
      local ey = node.y + rx.y * e.x + up.y * e.y + rz.y * e.z
      local ez = node.z + rx.z * e.x + up.z * e.y + rz.z * e.z
      local f = e.thrust * throttle
      assembly.forceAt(node, vec3(up.x * f, up.y * f, up.z * f), vec3(ex, ey, ez))
    end
  end

  -- Rate-damped attitude: command a turn rate on each vessel axis, torque
  -- toward it, damp everything else (a crude SAS).
  if not info.anchored then
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
  end

  -- SPACE: clamped → release the launch clamps; flying → fire the lowest
  -- remaining decoupler (every part at or below it detaches as a live stage).
  if input.pressed("space") then
    if info.anchored then
      -- Wait for the whole stack: releasing a half-assembled vessel would
      -- leave late parts spawning into a moving compound.
      if #info.parts >= part_total then
        assembly.setAnchored(node, false)
        log("launch clamps released")
      end
    elseif #decouplers > 0 then
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

  -- ---- status line --------------------------------------------------------
  if info.anchored then
    if #info.parts >= part_total then
      set_hud("CLAMPED   ·   SHIFT throttle   ·   SPACE — release clamps   ·   F — exit pod")
    else
      set_hud(string.format("assembling…  %d / %d parts", #info.parts, part_total))
    end
  else
    local v = info.vel
    local speed = math.sqrt(v.x * v.x + v.y * v.y + v.z * v.z)
    set_hud(string.format("THR %3d%%   %5.1f m/s   ·   SPACE stage   ·   F — exit pod",
      math.floor(throttle * 100 + 0.5), speed))
  end
end
