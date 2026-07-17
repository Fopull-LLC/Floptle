-- The ship (solar demo S3 v1): board it, fly it like a rocket, reach orbit,
-- land on the moon — or crash and wreck it.
--
--   F        board / exit (walk within `board_range` first)
--   SHIFT / CTRL   throttle up / down        X  cut throttle
--   W/S      pitch      A/D  yaw      Q/E  roll
--   T        SAS toggle (kills rotation when you let go — fly-by-wire)
--   G        respawn a wrecked ship at its landing spot
--
-- Real technique applies: thrust is along the ship's NOSE (+up when sitting on
-- the pad), F = ma with a real mass, gravity is µ/r² from the dominant body —
-- so getting to orbit means pitching over and building TANGENTIAL speed, and
-- landing means killing horizontal velocity first. The HUD (Console, 1/s while
-- piloting) reads altitude / speed / the conic you're on via space.elements.
--
-- Wrecks: an impact whose along-the-normal speed exceeds `crash_speed` (or
-- pulling more than `max_g` under thrust) breaks the ship — engines die. That
-- is v1 "structural stress"; parts/debris come with the prefab pass.

defaults = {
  mass = 2.0,          -- tonnes-ish; accel = thrust / mass
  max_thrust = 30.0,   -- so max accel ≈ 15 (planet surface g ≈ 7)
  rcs = 1.4,           -- attitude authority, rad/s²
  sas_damp = 5.0,      -- how hard SAS kills residual rotation
  crash_speed = 12.0,  -- impact speed along the surface normal that wrecks
  max_g = 9.0,         -- sustained accel that overstresses the frame
  board_range = 5.0,
  throttle_rate = 0.6, -- full throttle in ~1.7 s of held SHIFT
}

-- Published state (camera + walker + dig tool read these).
piloting = false
wrecked = false
throttle = 0.0

local sas = true
-- Ship basis (world space): nose = thrust axis, plus right/fwd to steer with.
local nx, ny, nz = 0.0, 1.0, 0.0
local fx, fy, fz = 0.0, 0.0, -1.0
local avp, avy, avr = 0.0, 0.0, 0.0 -- angular velocity about right/nose/fwd
local astronaut
local hud_t = -10.0
local pad_x, pad_y, pad_z

local function norm(x, y, z)
  local l = math.sqrt(x * x + y * y + z * z)
  if l < 1e-6 then return 0, 0, 0 end
  return x / l, y / l, z / l
end

local function cross(ax, ay, az, bx, by, bz)
  return ay * bz - az * by, az * bx - ax * bz, ax * by - ay * bx
end

-- Rotate vector v about unit axis k by angle a (Rodrigues).
local function rot(vx, vy, vz, kx, ky, kz, a)
  local c, s = math.cos(a), math.sin(a)
  local dx, dy, dz = cross(kx, ky, kz, vx, vy, vz)
  local d = kx * vx + ky * vy + kz * vz
  return vx * c + dx * s + kx * d * (1 - c),
         vy * c + dy * s + ky * d * (1 - c),
         vz * c + dz * s + kz * d * (1 - c)
end

function start(node)
  pad_x, pad_y, pad_z = node.x, node.y, node.z
  -- Nose = radially out from the dominant body (sitting upright on the pad).
  local d = space.dominant(node.x, node.y, node.z)
  local b = d and space.body(d)
  if b then
    nx, ny, nz = norm(node.x - b.x, node.y - b.y, node.z - b.z)
  end
  -- Any forward perpendicular to the nose.
  fx, fy, fz = norm(cross(nx, ny, nz, 1, 0, 0))
  if fx == 0 and fy == 0 and fz == 0 then
    fx, fy, fz = norm(cross(nx, ny, nz, 0, 0, 1))
  end
end

function fixedUpdate(node, dt)
  if not astronaut then astronaut = find("Astronaut") end

  -- ---- board / exit -------------------------------------------------------
  if input.pressed("f") and astronaut then
    if piloting then
      piloting = false
      local rx2, ry2, rz2 = cross(fx, fy, fz, nx, ny, nz)
      astronaut.x = node.x + rx2 * 3.5
      astronaut.y = node.y + ry2 * 3.5
      astronaut.z = node.z + rz2 * 3.5
      astronaut.vx, astronaut.vy, astronaut.vz = node.vx, node.vy, node.vz
      astronaut.visible = true
      throttle = 0.0
    elseif distance(astronaut, node) <= params.board_range then
      piloting = true
      astronaut.visible = false
    end
  end
  -- Parked astronaut rides inside the hull (bodies don't push each other).
  if piloting and astronaut then
    astronaut.x, astronaut.y, astronaut.z = node.x, node.y, node.z
    astronaut.vx, astronaut.vy, astronaut.vz = node.vx, node.vy, node.vz
  end

  if not piloting then return end

  -- ---- wreck & respawn ----------------------------------------------------
  if wrecked then
    throttle = 0.0
    if input.pressed("g") then
      wrecked = false
      node.x, node.y, node.z = pad_x, pad_y, pad_z
      node.vx, node.vy, node.vz = 0, 0, 0
      avp, avy, avr = 0, 0, 0
      start(node)
      print("ship restored at the pad")
    end
    return
  end

  if input.pressed("t") then
    sas = not sas
    print(sas and "SAS on" or "SAS off")
  end

  -- ---- throttle -----------------------------------------------------------
  if input.key("shift") then throttle = throttle + params.throttle_rate * dt end
  if input.key("ctrl") then throttle = throttle - params.throttle_rate * dt end
  if input.key("x") then throttle = 0.0 end
  if throttle > 1.0 then throttle = 1.0 end
  if throttle < 0.0 then throttle = 0.0 end

  -- ---- attitude (RCS torque → angular velocity → basis) -------------------
  local p = (input.key("s") and 1 or 0) - (input.key("w") and 1 or 0)
  local y = (input.key("a") and 1 or 0) - (input.key("d") and 1 or 0)
  local r = (input.key("e") and 1 or 0) - (input.key("q") and 1 or 0)
  avp = avp + p * params.rcs * dt
  avy = avy + y * params.rcs * dt
  avr = avr + r * params.rcs * dt
  if sas and p == 0 and y == 0 and r == 0 then
    local k = math.max(0.0, 1.0 - params.sas_damp * dt)
    avp, avy, avr = avp * k, avy * k, avr * k
  end
  local rx2, ry2, rz2 = cross(fx, fy, fz, nx, ny, nz) -- ship right
  -- ω = right·pitch + nose·yaw + fwd·roll, applied to the whole basis.
  local wx = rx2 * avp + nx * avy + fx * avr
  local wy = ry2 * avp + ny * avy + fy * avr
  local wz = rz2 * avp + nz * avy + fz * avr
  local wl = math.sqrt(wx * wx + wy * wy + wz * wz)
  if wl > 1e-6 then
    local ux2, uy2, uz2 = wx / wl, wy / wl, wz / wl
    nx, ny, nz = rot(nx, ny, nz, ux2, uy2, uz2, wl * dt)
    fx, fy, fz = rot(fx, fy, fz, ux2, uy2, uz2, wl * dt)
  end
  -- Re-orthonormalize (drift-proof): fwd ⊥ nose, unit.
  local d = fx * nx + fy * ny + fz * nz
  fx, fy, fz = norm(fx - nx * d, fy - ny * d, fz - nz * d)
  nx, ny, nz = norm(nx, ny, nz)

  -- ---- thrust: F = ma along the nose --------------------------------------
  local acc = throttle * params.max_thrust / params.mass
  if acc > params.max_g then
    wrecked = true
    print(string.format("STRUCTURAL FAILURE at %.1f g — ship wrecked (G to restore)", acc))
    return
  end
  node.vx = node.vx + nx * acc * dt
  node.vy = node.vy + ny * acc * dt
  node.vz = node.vz + nz * acc * dt

  -- ---- write the node's orientation (nose = node +Y, fwd = node −Z) -------
  local yaw2 = math.atan2(-fx, -fz)
  local pit2 = math.asin(math.max(-1, math.min(1, fy)))
  local du = nx * fx + ny * fy + nz * fz
  local wx2, wy2, wz2 = norm(nx - fx * du, ny - fy * du, nz - fz * du)
  if wx2 == 0 and wy2 == 0 and wz2 == 0 then wx2, wy2, wz2 = 0, 1, 0 end
  local cy2, sy2 = math.cos(-yaw2), math.sin(-yaw2)
  local ax = wx2 * cy2 + wz2 * sy2
  local ay = wy2
  local az = -wx2 * sy2 + wz2 * cy2
  local cx2, sx2 = math.cos(-pit2), math.sin(-pit2)
  local by = ay * cx2 - az * sx2
  node.yaw, node.pitch, node.roll = yaw2, pit2, math.atan2(-ax, by)

  -- ---- HUD: 1/s to the Console -------------------------------------------
  if time - hud_t >= 1.0 then
    hud_t = time
    local dom = space.dominant(node.x, node.y, node.z)
    local b = dom and space.body(dom)
    if b then
      local alt = distance(node, { x = b.x, y = b.y, z = b.z }) - b.radius
      local spd = math.sqrt(node.vx ^ 2 + node.vy ^ 2 + node.vz ^ 2)
      local o = space.elements(node.x, node.y, node.z, node.vx, node.vy, node.vz)
      local orbit = ""
      if o and o.apoapsis then
        orbit = string.format("  pe %.0f / ap %.0f (T %.0fs)",
          o.periapsis - b.radius, o.apoapsis - b.radius, o.period)
      elseif o then
        orbit = string.format("  ESCAPE (pe %.0f)", o.periapsis - b.radius)
      end
      print(string.format("[%s] alt %.0f  spd %.1f  thr %.0f%%%s", dom, alt, spd, throttle * 100, orbit))
    end
  end
end

-- An impact hard enough along the surface normal breaks the ship.
function onCollisionEnter(node, other, hit)
  if wrecked then return end
  local vn = math.abs(node.vx * hit.nx + node.vy * hit.ny + node.vz * hit.nz)
  if vn > params.crash_speed then
    wrecked = true
    throttle = 0.0
    print(string.format("CRASH at %.1f m/s — ship wrecked (board + G to restore)", vn))
  end
end
