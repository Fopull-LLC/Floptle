-- VESSEL SPAWNER — when the scene loads with `shipyard.launch` set, rebuild
-- the saved blueprint as a LIVE compound assembly beside the pad: one Vessel
-- root (RigidBody assembly) + every blueprint part spawned as its child, then
-- `assembly.rebuild` gathers them into the physics compound. The blueprint is
-- self-contained — no builder registry here.
--
-- Planets are ROUND: the vessel is placed against the actual surface (a
-- raycast along local gravity-up) and TILTED so its stack axis is radial —
-- spawning axis-aligned 14 units around a curved planet would bury half the
-- ship and catapult it (the honest physics obliges).

defaults = {
  offset = 14.0,   -- sideways distance from the pad
  clear = 0.4,     -- gap between the lowest part and the ground at spawn
}

local pending = 0
local vessel_node = nil

-- The body whose SOI we're inside (nearest wins when SOIs nest — a moon
-- close-by beats its planet).
local function dominant_at(x, y, z)
  local best, bd = nil, nil
  for _, b in ipairs(space.bodies()) do
    local dx, dy, dz = x - b.x, y - b.y, z - b.z
    local d = math.sqrt(dx * dx + dy * dy + dz * dz)
    if (b.soi or -1) > 0 and d < b.soi and (not best or d < bd) then
      best, bd = b, d
    end
  end
  return best
end

-- Vessel basis for up alignment: yaw = 0, R = Rx(pitch)·Rz(roll); the +Y
-- column is (−sin r, cos r·cos p, cos r·sin p), so a target up solves to
-- roll = asin(−ux), pitch = atan2(uz, uy).
local function up_angles(ux, uy, uz)
  local roll = math.asin(math.max(-1, math.min(1, -ux)))
  local pitch = math.atan2(uz, uy)
  return pitch, roll
end

local function basis(pitch, roll)
  local cx, sx = math.cos(pitch), math.sin(pitch)
  local cz, sz = math.cos(roll), math.sin(roll)
  local rx = vec3(cz, cx * sz, sx * sz)
  local up = vec3(-sz, cx * cz, sx * cz)
  local rz = vec3(0, -sx, cx)
  return rx, up, rz
end

local function spawn_parts(vessel, bp, pitch, roll)
  local rxv, upv, rzv = basis(pitch, roll)
  local vx, vy, vz = vessel.x, vessel.y, vessel.z
  local total = 0
  for _, d in pairs(bp.parts) do
    total = total + 1
    pending = pending + 1
    local wx = vx + rxv.x * d.x + upv.x * d.y + rzv.x * d.z
    local wy = vy + rxv.y * d.x + upv.y * d.y + rzv.y * d.z
    local wz = vz + rxv.z * d.x + upv.z * d.y + rzv.z * d.z
    spawn(d.prefab, vec3(wx, wy, wz), function(part)
      part.yaw = d.yaw or 0
      pending = pending - 1
      if pending == 0 and vessel_node then
        assembly.rebuild(vessel_node)
        log("vessel assembled: " .. total .. " parts on the pad")
      end
    end, vessel)
  end
end

function start(node)
  if save.get("shipyard.launch") ~= 1 then return end
  save.set("shipyard.launch", 0)
  local bp = save.get("shipyard.blueprint")
  if not bp or not bp.parts then return end

  local pad = find("Ship")
  local px, py, pz = 0, 20, 0
  if pad then px, py, pz = pad.x, pad.y, pad.z end

  -- Local up: radial from the dominant body (fallback: world +Y).
  local ux, uy, uz = 0, 1, 0
  local body = dominant_at(px, py, pz)
  if body then
    local dx, dy, dz = px - body.x, py - body.y, pz - body.z
    local d = math.sqrt(dx * dx + dy * dy + dz * dz)
    if d > 1e-3 then ux, uy, uz = dx / d, dy / d, dz / d end
  end
  -- A sideways direction perpendicular to up (project world X out of up).
  local sx, sy, sz = 1 - ux * ux, -ux * uy, -ux * uz
  local sl = math.sqrt(sx * sx + sy * sy + sz * sz)
  if sl < 1e-3 then sx, sy, sz, sl = 0, 0, 1, 1 end
  sx, sy, sz = sx / sl, sy / sl, sz / sl

  -- Aim point beside the pad, then find the actual ground under it.
  local ax = px + sx * params.offset + ux * 6
  local ay = py + sy * params.offset + uy * 6
  local az = pz + sz * params.offset + uz * 6
  local gx, gy, gz = ax, ay, az
  local hit = raycast(ax, ay, az, -ux, -uy, -uz, 220.0)
  if hit and hit.distance then
    gx = ax - ux * (hit.distance - params.clear)
    gy = ay - uy * (hit.distance - params.clear)
    gz = az - uz * (hit.distance - params.clear)
  end

  local pitch, roll = up_angles(ux, uy, uz)
  spawn("Vessel", vec3(gx, gy, gz), function(v)
    v.pitch, v.roll, v.yaw = pitch, roll, 0
    vessel_node = v
    spawn_parts(v, bp, pitch, roll)
  end)
end
