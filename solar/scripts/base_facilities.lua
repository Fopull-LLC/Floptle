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

-- THE BASE IS A PLACE. It is sited ONCE — at the crew's landing site, and only
-- after the loading screen has handed the world over — and then remembered
-- body-relative in the save (`base.body` + `base.x/y/z`). Every later session
-- rebuilds it on the same ground.
--
-- Before this the ring was cut around wherever the astronaut happened to be
-- when the terrain first answered, and on a loaded save that is the LOADING
-- HOVER: game_manager parks the crew 60 m over the spawn planet's north pole
-- while the field streams, THEN teleports them to their saved position. The
-- colony went up at the pole and the player woke up hundreds of metres away —
-- with no prompt to enter anything, because they were never actually near a
-- building.
--
-- angle = bearing around the base (deg); radius = ground distance from the base
-- site; seat = how far the prefab's ORIGIN sits above the ground (= |mesh y-min| ×
-- prefab scale, so the building's base rests on the surface); yaw = spin about the
-- local up for facing variety.
--
-- The three SHEDS (command centre, assembly hangar, depot) are walk-in
-- buildings: their prefabs carry a SHELL collider built from the model's own
-- geometry rather than one box around the whole thing, so the doorways the mesh
-- has are doorways you can use. The power plant and the tracking dish are
-- machines and stay solid. Bearings dodge 0° (where the launchpad drops during
-- a launch) and keep the buildings clear of each other.
local FACILITIES = {
  { prefab = "FacCommand",  angle = 40,  radius = 30, seat = 2.1,   yaw = 215 },
  { prefab = "FacHangar",   angle = 145, radius = 30, seat = 3.5,   yaw = 330 },
  { prefab = "FacPower",    angle = 250, radius = 24, seat = 1.02,  yaw = 70 },
  { prefab = "FacTracking", angle = 320, radius = 33, seat = 1.64,  yaw = 130 },
  -- The Commerce Depot: cargo comes off a landed craft here and materials are
  -- sold from here. Sited between the command centre and the hangar, on the
  -- side you walk back to from the pad.
  { prefab = "FacDepot",    angle = 90,  radius = 28, seat = 1.25,  yaw = 250 },
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

-- Which world the base belongs to. Once sited it is THAT planet's colony: if
-- the crew is off visiting another world nothing is built (and nothing tries to
-- stream a planet's terrain from across the system to do it) — the base is
-- waiting for them at home.
local function site_body(crew)
  local home = save.get("base.body")
  local here = dominant_at(crew.x, crew.y, crew.z)
  if home then
    if not here or here.name ~= home then return nil end
    return space.body(home) or here
  end
  return here
end

-- The base's centre, in WORLD coordinates, recomputed from the body's live
-- position every attempt (the planet is orbiting while we work). Returns the
-- site, its up vector, and whether it came from the save.
local function site_point(body, crew)
  local hx, hy, hz = save.get("base.x"), save.get("base.y"), save.get("base.z")
  if save.get("base.body") == body.name and hx and hy and hz then
    local px, py, pz = body.x + hx, body.y + hy, body.z + hz
    local d = math.sqrt(hx * hx + hy * hy + hz * hz)
    if d > 1e-3 then return px, py, pz, hx / d, hy / d, hz / d, true end
  end
  local dx, dy, dz = crew.x - body.x, crew.y - body.y, crew.z - body.z
  local d = math.sqrt(dx * dx + dy * dy + dz * dz)
  if d < 1e-3 then return crew.x, crew.y, crew.z, 0, 1, 0, false end
  return crew.x, crew.y, crew.z, dx / d, dy / d, dz / d, false
end

local function try_place()
  -- Nothing is sited while the loading screen is up: the crew is parked over
  -- the north pole then, not standing where they live.
  local gm = findScript("game_manager")
  if gm and gm.loading then return false end

  -- Base site = the crew spawn (the Astronaut). Fall back to the debug Ship.
  local crew = find("Astronaut") or find("Ship")
  if not crew then return false end

  local body = site_body(crew)
  if not body then return false end
  if body.name and body.name ~= "" then terrain.warm(body.name) end
  local px, py, pz, ux, uy, uz, from_save = site_point(body, crew)

  -- Pin the site to the GROUND under it before anything is built: the probe
  -- doubles as the "is the terrain actually here yet?" gate, and a crew still
  -- falling out of a restore doesn't drag the colony down with them.
  local centre = raycast(px + ux * 240, py + uy * 240, pz + uz * 240,
                         -ux, -uy, -uz, 600.0)
  if not (centre and centre.distance) then return false end
  px = px + ux * 240 - ux * centre.distance
  py = py + uy * 240 - uy * centre.distance
  pz = pz + uz * 240 - uz * centre.distance
  if not from_save then
    -- Remember it body-relative — absolute coordinates go stale the moment the
    -- planet moves on its orbit.
    save.set("base.body", body.name)
    save.set("base.x", px - body.x)
    save.set("base.y", py - body.y)
    save.set("base.z", pz - body.z)
    log(string.format("base: colony sited on %s (this is home now)", body.name))
  end

  -- Two tangents spanning the ground plane at the base: s = world-X projected out
  -- of up, t = up × s. Facilities ring the base along these.
  local sx, sy, sz = 1 - ux * ux, -ux * uy, -ux * uz
  local sl = math.sqrt(sx * sx + sy * sy + sz * sz)
  if sl < 1e-3 then sx, sy, sz, sl = 0, 0, 1, 1 end
  sx, sy, sz = sx / sl, sy / sl, sz / sl
  local tx = uy * sz - uz * sy
  local ty = uz * sx - ux * sz
  local tz = ux * sy - uy * sx

  -- No planet node, no base. A facility that fails to parent keeps WORLD
  -- coordinates: it never rides the orbit (the ground slides out from under it)
  -- and it reads in a different frame from everything that measures against it.
  -- The rails snapshot and the scene mirror can disagree for a frame right after
  -- the generator rolls a system, so this waits rather than building wrong.
  local planet_node = find(body.name)
  if not planet_node then return false end
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
    -- How far you are from your own front door, said out loud: a base that is
    -- half a planet away is a fact worth knowing before you go looking for it.
    local crew = find("Astronaut")
    local b = space.body(save.get("base.body") or "")
    local hx, hy, hz = save.get("base.x"), save.get("base.y"), save.get("base.z")
    if crew and b and hx then
      local dx, dy, dz = crew.x - (b.x + hx), crew.y - (b.y + hy), crew.z - (b.z + hz)
      log(string.format("base: all facilities standing — %.0f m from the crew",
        math.sqrt(dx * dx + dy * dy + dz * dz)))
    else
      log("base: all facilities standing")
    end
  elseif wait_t - last_note > 5.0 then
    last_note = wait_t
    log(string.format("base: waiting for terrain… (%.0fs, %d/%d placed)", wait_t, placed, #FACILITIES))
  end
end
