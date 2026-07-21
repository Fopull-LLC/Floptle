-- VESSEL SPAWNER — when the scene loads with `shipyard.launch` set, rebuild
-- the saved blueprint as a LIVE compound assembly on a real launchpad: pad
-- down first, then one Vessel root (RigidBody assembly) + every blueprint part
-- as its child, then `assembly.rebuild` gathers them into the physics compound
-- and CLAMPS it (anchored) until the pilot releases. The blueprint is
-- self-contained — no builder registry here.
--
-- Planets are ROUND and they ORBIT: everything here is positioned RELATIVE to
-- the dominant body's live rails position. The launchpad spawns PARENTED to
-- the planet node (it rides the orbit through the transform hierarchy; the
-- engine carries its static collider), and while the vessel is clamped it is
-- re-pinned to the pad deck every tick — nothing can drift apart while the
-- planet sails along its orbit.

defaults = {
  offset = 14.0,   -- sideways distance from the pad
  clear = 0.4,     -- gap between the lowest part and the deck at spawn
}

local pending = 0
local vessel_node = nil
local body_name = nil       -- the dominant body we spawned against
local relv = nil            -- vessel origin, body-relative (the clamp pin)
local assemble_t = nil      -- when part spawning began (stall self-heal)
local rebuilt = false

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
      -- The engine parents a spawned part by converting its WORLD pose into
      -- the vessel's local frame — which leaves inv(vessel tilt) baked into
      -- the part's local rotation (the "misrotated parts" of every sloped
      -- launch). Parts are authored UPRIGHT in the vessel frame: zero the
      -- inherited tilt, keep only the blueprint yaw.
      part.pitch, part.roll, part.yaw = 0, 0, d.yaw or 0
      pending = pending - 1
      if pending == 0 and vessel_node then
        rebuilt = true
        assembly.rebuild(vessel_node)
        -- CLAMPED from the first physics tick: anchored (no gravity, no
        -- contacts, rides the planet's frame) until the pilot releases —
        -- nothing can fling a vessel that is not yet simulating.
        assembly.setAnchored(vessel_node, true)
        log("vessel assembled: " .. total .. " parts — clamped to the pad")
      end
    end, vessel)
  end
  assemble_t = time
end

-- The spawn WAITS for solid ground: at scene start the pad's terrain may not
-- be streamed in yet — spawning early drops the vessel through the world.
local want_spawn = false
local bp = nil
local wait_t = 0.0
local last_note = 0.0

function start(node)
  if save.get("shipyard.launch") ~= 1 then
    save.set("shipyard.pilot", 0) -- a stale handoff flag must not park the walker
    return
  end
  save.set("shipyard.launch", 0)
  bp = save.get("shipyard.blueprint")
  if not bp or not bp.parts then
    save.set("shipyard.pilot", 0)
    return
  end
  want_spawn = true
  log("launch: vessel blueprint aboard — waiting for the pad terrain…")
end

local function try_spawn()
  -- Site reference read FRESH each attempt: the ASTRONAUT (the crew spawn —
  -- the scout "Ship" node is a stashed debug tool now and may be parked in
  -- deep space).
  local pad = find("Astronaut") or find("Ship")
  local px, py, pz = 0, 20, 0
  if pad then px, py, pz = pad.x, pad.y, pad.z end

  -- Local up: radial from the dominant body (fallback: world +Y). Keep its
  -- terrain WARM so the ground actually streams in under us.
  local ux, uy, uz = 0, 1, 0
  local body = dominant_at(px, py, pz)
  if body then
    if body.name and body.name ~= "" then terrain.warm(body.name) end
    local dx, dy, dz = px - body.x, py - body.y, pz - body.z
    local d = math.sqrt(dx * dx + dy * dy + dz * dz)
    if d > 1e-3 then ux, uy, uz = dx / d, dy / d, dz / d end
  end
  -- A sideways direction perpendicular to up (project world X out of up).
  local sx, sy, sz = 1 - ux * ux, -ux * uy, -ux * uz
  local sl = math.sqrt(sx * sx + sy * sy + sz * sz)
  if sl < 1e-3 then sx, sy, sz, sl = 0, 0, 1, 1 end
  sx, sy, sz = sx / sl, sy / sl, sz / sl

  -- Ground check beside the pad; WAIT (warming) until it answers.
  local ax = px + sx * params.offset + ux * 8
  local ay = py + sy * params.offset + uy * 8
  local az = pz + sz * params.offset + uz * 8
  local hit = raycast(ax, ay, az, -ux, -uy, -uz, 260.0)
  if not (hit and hit.distance) then
    if wait_t - (last_note or 0) > 5.0 then
      last_note = wait_t
      log(string.format("launch: waiting for terrain… (%.0fs)", wait_t))
    end
    return false
  end
  local gx = ax - ux * hit.distance
  local gy = ay - uy * hit.distance
  local gz = az - uz * hit.distance

  -- Everything below is BODY-RELATIVE: the ground point as an offset from the
  -- planet's center right now. World positions recompute from the LIVE body
  -- each time they're needed — the planet is orbiting while we work.
  local relg = body and { x = gx - body.x, y = gy - body.y, z = gz - body.z } or nil
  body_name = body and body.name or nil
  local pitch, roll = up_angles(ux, uy, uz)
  local deck = 1.24

  -- The LAUNCHPAD spawns PARENTED to the planet node: it rides the orbit
  -- through the transform hierarchy, and the engine keeps its baked static
  -- collider glued to the moving surface. Its local position is set exact in
  -- the callback (body-relative — no tick-of-drift error).
  local planet_node = body_name and find(body_name) or nil
  spawn("Launchpad", vec3(gx + ux * deck, gy + uy * deck, gz + uz * deck), function(lp)
    lp.pitch, lp.roll, lp.yaw = pitch, roll, 0
    if planet_node and relg then
      -- Node coordinates are PARENT-LOCAL: for a pad parented to its planet,
      -- local IS the body-relative offset. (Adding the planet position here
      -- put the pad on the far side of the star system — round 7's missing
      -- launchpad.)
      lp.x = relg.x + ux * deck
      lp.y = relg.y + uy * deck
      lp.z = relg.z + uz * deck
    end
    -- The vessel assembles on the deck (deck top ~1.24 above the pad center),
    -- positioned from the LIVE body so no orbit-drift creeps in between the
    -- raycast tick and this one.
    local b2 = body_name and space.body(body_name)
    local bx, by, bz = gx, gy, gz
    if b2 and relg then bx, by, bz = b2.x + relg.x, b2.y + relg.y, b2.z + relg.z end
    local vx = bx + ux * (deck * 2.0 + params.clear)
    local vy = by + uy * (deck * 2.0 + params.clear)
    local vz = bz + uz * (deck * 2.0 + params.clear)
    if b2 then relv = { x = vx - b2.x, y = vy - b2.y, z = vz - b2.z } end
    spawn("Vessel", vec3(vx, vy, vz), function(v)
      v.pitch, v.roll, v.yaw = pitch, roll, 0
      vessel_node = v
      spawn_parts(v, bp, pitch, roll)
    end)
  end, planet_node)
  log("launch: pad down, vessel assembling")
  return true
end

function update(node, dt)
  if want_spawn then
    wait_t = wait_t + dt
    if try_spawn() then want_spawn = false end
    return
  end

  -- Stall self-heal: a part whose spawn never lands (bad prefab name in an
  -- old blueprint) must not wedge the whole launch — build the compound from
  -- whatever DID arrive and say so.
  if not rebuilt and assemble_t and pending > 0 and time - assemble_t > 8.0
    and vessel_node and vessel_node.valid then
    rebuilt = true
    assembly.rebuild(vessel_node)
    assembly.setAnchored(vessel_node, true)
    log(string.format("launch: %d part(s) never spawned — assembled without them", pending))
  end

  -- While CLAMPED, the vessel is re-pinned to its pad-deck spot every tick
  -- (body-relative): the anchored compound and the planet-parented pad both
  -- ride the orbit, and the pin closes any residual drift between the two
  -- carrying mechanisms. Stops the moment the pilot releases the clamps.
  if vessel_node and vessel_node.valid and relv and body_name then
    local info = assembly.info(vessel_node)
    if info and info.anchored then
      local b = space.body(body_name)
      if b then
        assembly.teleport(vessel_node,
          vec3(b.x + relv.x, b.y + relv.y, b.z + relv.z))
      end
    elseif info then
      relv = nil -- released: the pin's job is done
    end
  end
end
