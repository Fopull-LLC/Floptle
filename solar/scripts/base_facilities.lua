-- BASE FACILITIES — the permanent colony buildings at the home base. Unlike the
-- launchpad (which the vessel_spawner drops only for a launch), these stand at the
-- base for the whole session: a command centre, a vehicle-assembly hangar, a power
-- plant, and the tracking station (the comms dish the facilities menu reads the
-- `comms.ships` registry through).
--
-- Placement mirrors vessel_spawner exactly: planets are ROUND and ORBIT, so each
-- facility is positioned RELATIVE to the dominant body, seated on the surface by a
-- downward raycast, tilted to the local surface normal, and spawned PARENTED to the
-- planet node — it rides the orbit through the transform hierarchy and the engine
-- keeps its baked Static collider glued to the moving ground. Coordinates are
-- recomputed from the LIVE body each attempt so no orbit drift creeps in.

-- angle = bearing around the base (deg); radius = ground distance from the crew
-- spawn; seat = how far the prefab's ORIGIN sits above the ground (= |mesh y-min| ×
-- prefab scale, so the building's base rests on the surface); yaw = spin about the
-- local up for facing variety. Bearings dodge 0° (where the launchpad drops during
-- a launch) and keep the buildings clear of each other.
local FACILITIES = {
  { prefab = "FacCommand",  angle = 40,  radius = 26, seat = 1.5,   yaw = 215 },
  { prefab = "FacHangar",   angle = 145, radius = 30, seat = 1.875, yaw = 330 },
  { prefab = "FacPower",    angle = 250, radius = 24, seat = 1.02,  yaw = 70 },
  { prefab = "FacTracking", angle = 320, radius = 33, seat = 1.64,  yaw = 130 },
}

local want_spawn = false
local wait_t = 0.0
local last_note = 0.0
local done = {}      -- prefab -> true once placed
local placed = 0

-- The body whose SOI we're inside (nearest wins when SOIs nest).
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

-- Vessel/prefab basis for up alignment (see vessel_spawner): yaw = 0,
-- R = Rx(pitch)·Rz(roll); target up solves to roll = asin(-ux), pitch = atan2(uz, uy).
local function up_angles(ux, uy, uz)
  local roll = math.asin(math.max(-1, math.min(1, -ux)))
  local pitch = math.atan2(uz, uy)
  return pitch, roll
end

function start(node)
  -- Wait for the crew + terrain to exist, then place the base once.
  want_spawn = true
  log("base: siting facilities — waiting for the base terrain…")
end

local function try_place()
  -- Base site = the crew spawn (the Astronaut). Fall back to the debug Ship.
  local crew = find("Astronaut") or find("Ship")
  if not crew then return false end
  local px, py, pz = crew.x, crew.y, crew.z

  -- Local up = radial from the dominant body (fallback world +Y); keep its
  -- terrain warm so the ground streams in under the base.
  local ux, uy, uz = 0, 1, 0
  local body = dominant_at(px, py, pz)
  if body then
    if body.name and body.name ~= "" then terrain.warm(body.name) end
    local dx, dy, dz = px - body.x, py - body.y, pz - body.z
    local d = math.sqrt(dx * dx + dy * dy + dz * dz)
    if d > 1e-3 then ux, uy, uz = dx / d, dy / d, dz / d end
  end
  if not body then return false end

  -- Two tangents spanning the ground plane at the base: s = world-X projected out
  -- of up, t = up × s. Facilities ring the base along these.
  local sx, sy, sz = 1 - ux * ux, -ux * uy, -ux * uz
  local sl = math.sqrt(sx * sx + sy * sy + sz * sz)
  if sl < 1e-3 then sx, sy, sz, sl = 0, 0, 1, 1 end
  sx, sy, sz = sx / sl, sy / sl, sz / sl
  local tx = uy * sz - uz * sy
  local ty = uz * sx - ux * sz
  local tz = ux * sy - uy * sx

  local planet_node = find(body.name)
  local pitch, roll = up_angles(ux, uy, uz)

  for _, f in ipairs(FACILITIES) do
    if not done[f.prefab] then
      local a = math.rad(f.angle)
      -- Ring direction in the ground plane, then the probe point above it.
      local dx = math.cos(a) * sx + math.sin(a) * tx
      local dy = math.cos(a) * sy + math.sin(a) * ty
      local dz = math.cos(a) * sz + math.sin(a) * tz
      local ax = px + dx * f.radius + ux * 12
      local ay = py + dy * f.radius + uy * 12
      local az = pz + dz * f.radius + uz * 12
      local hit = raycast(ax, ay, az, -ux, -uy, -uz, 360.0)
      if hit and hit.distance then
        -- Ground point, then origin seated above it; body-relative for the
        -- planet-local coordinates the parent expects.
        local gx = ax - ux * hit.distance
        local gy = ay - uy * hit.distance
        local gz = az - uz * hit.distance
        local ox = gx + ux * f.seat
        local oy = gy + uy * f.seat
        local oz = gz + uz * f.seat
        local rel = { x = ox - body.x, y = oy - body.y, z = oz - body.z }
        local fyaw = f.yaw
        spawn(f.prefab, vec3(ox, oy, oz), function(inst)
          inst.pitch, inst.roll, inst.yaw = pitch, roll, fyaw
          if planet_node then
            -- Parent-local coords: for a facility parented to its planet, local
            -- IS the body-relative offset (never add the body position — that
            -- flings it to the far side of the system).
            inst.x, inst.y, inst.z = rel.x, rel.y, rel.z
          end
        end, planet_node)
        done[f.prefab] = true
        placed = placed + 1
        log("base: " .. f.prefab .. " sited (" .. placed .. "/" .. #FACILITIES .. ")")
      end
    end
  end
  return placed >= #FACILITIES
end

function update(node, dt)
  if not want_spawn then return end
  wait_t = wait_t + dt
  if try_place() then
    want_spawn = false
    log("base: all facilities standing")
  elseif wait_t - last_note > 5.0 then
    last_note = wait_t
    log(string.format("base: waiting for terrain… (%.0fs, %d/%d placed)", wait_t, placed, #FACILITIES))
  end
end
