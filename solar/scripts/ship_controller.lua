-- The ship (solar demo S3 v2): fly like KSP — rate-commanded attitude, real
-- F = ma under µ/r² gravity, a parking brake, honest crash detection, an
-- engine plume that follows the throttle, and a live HUD.
--
--   F        board / exit (walk within `board_range`)
--   SHIFT / CTRL   throttle up / down        X  cut throttle       Z  full
--   W/S      pitch      A/D  yaw      Q/E  roll   (hold = turn, release = stop)
--   T        SAS toggle       G  (while wrecked) restore at the pad
--
-- Attitude is RATE-COMMANDED, the way KSP's stability assist feels: holding a
-- key commands a turn RATE (ramped fast), releasing commands zero and the SAS
-- drives the rate back to nothing. No momentum fishing.
--
-- Crash detection uses the PREVIOUS tick's velocity (the solver has already
-- absorbed the impact by the time the collision event fires, so reading the
-- current velocity always says ~0 — the "slammed into a planet, nothing
-- happened" bug), plus a spawn grace period so settling onto the pad after
-- Play starts can't count as a crash.

defaults = {
  mass = 2.0,           -- tonnes-ish; accel = thrust / mass
  max_thrust = 44.0,    -- max accel 22 vs surface g 9.8 → TWR ≈ 2.2
  max_rate = 1.0,       -- commanded turn rate cap, rad/s
  rate_accel = 2.5,     -- how fast the rate ramps to the command, rad/s²
  crash_speed = 15.0,   -- impact speed along the normal that wrecks
  max_g = 30.0,         -- frame stress limit (headroom above max accel = 22)
  board_range = 6.0,
  throttle_rate = 0.5,  -- full throttle in 2 s of held SHIFT
  park_speed = 1.8,     -- below this, landed + idle = parked (pinned still)
  grace = 3.0,          -- seconds after spawn/restore with no crash detection
}

-- Published state (camera / walker / dig tool read these).
piloting = false
wrecked = false
throttle = 0.0

local sas = true
-- Ship basis (world space): nose = thrust axis; fwd/right complete the frame.
local nx, ny, nz = 0.0, 1.0, 0.0
local fx, fy, fz = 0.0, 0.0, -1.0
local avp, avy, avr = 0.0, 0.0, 0.0 -- angular rates about right/nose/fwd
local astronaut, flame, hud
local hud_t = -10.0
local pad_x, pad_y, pad_z
local spawn_t = -100.0
-- Previous tick's velocity — the honest pre-impact speed for crash checks.
local pvx, pvy, pvz = 0.0, 0.0, 0.0

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

-- Move `have` toward `want` by at most `step` (the rate controller).
local function toward(have, want, step)
  local d = want - have
  if d > step then return have + step end
  if d < -step then return have - step end
  return want
end

local function reset_pose(node)
  node.vx, node.vy, node.vz = 0, 0, 0
  avp, avy, avr = 0, 0, 0
  throttle = 0.0
  spawn_t = time
  -- Nose = radially out from the dominant body (upright on the ground).
  local d = space.dominant(node.x, node.y, node.z)
  local b = d and space.body(d)
  if b then
    nx, ny, nz = norm(node.x - b.x, node.y - b.y, node.z - b.z)
  else
    nx, ny, nz = 0, 1, 0
  end
  fx, fy, fz = norm(cross(nx, ny, nz, 1, 0, 0))
  if fx == 0 and fy == 0 and fz == 0 then
    fx, fy, fz = norm(cross(nx, ny, nz, 0, 0, 1))
  end
end

function start(node)
  pad_x, pad_y, pad_z = node.x, node.y, node.z
  reset_pose(node)
end

local function set_flame(node, on, pct)
  if not flame then flame = find("Ship Flame") end
  if not flame then return end
  local ps = flame:particles()
  if on and not ps:isPlaying() then ps:play() end
  if not on and ps:isPlaying() then ps:stop() end
  -- The plume's density AND particle size follow the throttle.
  if on then ps:setIntensity(0.25 + pct * 1.25) end
  local light = flame:getcomponent("PointLight")
  if light then light.intensity = on and (0.8 + pct * 4.0) or 0.0 end
end

local function set_hud(node, text)
  if not hud then hud = find("Ship HUD Text") end
  if not hud then return end
  local el = hud:getcomponent("UiElement")
  if el then el.visible = text ~= nil end
  if text then hud.text = text end
end

function fixedUpdate(node, dt)
  if not astronaut then astronaut = find("Astronaut") end

  -- ---- board / exit -------------------------------------------------------
  if input.pressed("f") and astronaut then
    if piloting then
      piloting = false
      local rx2, ry2, rz2 = cross(fx, fy, fz, nx, ny, nz)
      -- Step out beside the hatch, biased along the nose so a slope-landed
      -- ship doesn't drop you inside the hillside.
      astronaut.x = node.x + rx2 * 3.5 + nx * 1.0
      astronaut.y = node.y + ry2 * 3.5 + ny * 1.0
      astronaut.z = node.z + rz2 * 3.5 + nz * 1.0
      astronaut.vx, astronaut.vy, astronaut.vz = node.vx, node.vy, node.vz
      astronaut.visible = true
      throttle = 0.0
      set_flame(node, false, 0)
      set_hud(node, nil)
    elseif distance(astronaut, node) <= params.board_range then
      piloting = true
      astronaut.visible = false
      spawn_t = math.max(spawn_t, time - params.grace + 0.75) -- brief settle grace
    end
  end
  -- Parked astronaut rides inside the hull (bodies don't push each other).
  if piloting and astronaut then
    astronaut.x, astronaut.y, astronaut.z = node.x, node.y, node.z
    astronaut.vx, astronaut.vy, astronaut.vz = node.vx, node.vy, node.vz
  end

  if not piloting then
    pvx, pvy, pvz = node.vx, node.vy, node.vz
    return
  end

  -- ---- wreck & respawn ----------------------------------------------------
  if wrecked then
    set_flame(node, false, 0)
    set_hud(node, "SHIP WRECKED — press G to restore at the pad, F to exit")
    if input.pressed("g") then
      wrecked = false
      node.x, node.y, node.z = pad_x, pad_y, pad_z + 0.0
      reset_pose(node)
      print("ship restored at the pad")
    end
    pvx, pvy, pvz = node.vx, node.vy, node.vz
    return
  end

  if input.pressed("t") then sas = not sas end

  -- ---- throttle -----------------------------------------------------------
  if input.key("shift") then throttle = throttle + params.throttle_rate * dt end
  if input.key("ctrl") then throttle = throttle - params.throttle_rate * dt end
  if input.key("x") then throttle = 0.0 end
  if input.key("z") then throttle = 1.0 end
  if throttle > 1.0 then throttle = 1.0 end
  if throttle < 0.0 then throttle = 0.0 end

  -- ---- attitude: RATE-COMMANDED (the KSP feel) ----------------------------
  local p = (input.key("s") and 1 or 0) - (input.key("w") and 1 or 0)
  local y = (input.key("a") and 1 or 0) - (input.key("d") and 1 or 0)
  local r = (input.key("e") and 1 or 0) - (input.key("q") and 1 or 0)
  local step = params.rate_accel * dt
  -- SAS off = rates persist when released (pure Newton, for the purists).
  local hold = sas and 0 or nil
  avp = toward(avp, p ~= 0 and p * params.max_rate or (hold or avp), step)
  avy = toward(avy, y ~= 0 and y * params.max_rate or (hold or avy), step)
  avr = toward(avr, r ~= 0 and r * params.max_rate or (hold or avr), step)

  local rx2, ry2, rz2 = cross(fx, fy, fz, nx, ny, nz) -- ship right
  local wx = rx2 * avp + nx * avy + fx * avr
  local wy = ry2 * avp + ny * avy + fy * avr
  local wz = rz2 * avp + nz * avy + fz * avr
  local wl = math.sqrt(wx * wx + wy * wy + wz * wz)
  if wl > 1e-6 then
    local ux2, uy2, uz2 = wx / wl, wy / wl, wz / wl
    nx, ny, nz = rot(nx, ny, nz, ux2, uy2, uz2, wl * dt)
    fx, fy, fz = rot(fx, fy, fz, ux2, uy2, uz2, wl * dt)
  end
  local dd = fx * nx + fy * ny + fz * nz
  fx, fy, fz = norm(fx - nx * dd, fy - ny * dd, fz - nz * dd)
  nx, ny, nz = norm(nx, ny, nz)

  -- ---- thrust + parking brake --------------------------------------------
  local acc = throttle * params.max_thrust / params.mass
  if acc > params.max_g then
    wrecked = true
    spawnEffect("Explosion", node.x, node.y, node.z)
    print(string.format("STRUCTURAL FAILURE at %.1f g — ship wrecked (G to restore)", acc))
    return
  end
  local spd = math.sqrt(node.vx ^ 2 + node.vy ^ 2 + node.vz ^ 2)
  if node.grounded and throttle < 0.01 and spd < params.park_speed then
    -- Parked: pin it still. Kills the low-speed grinding/sliding jitter a
    -- sphere hull otherwise does on voxel terrain.
    node.vx, node.vy, node.vz = 0, 0, 0
  else
    node.vx = node.vx + nx * acc * dt
    node.vy = node.vy + ny * acc * dt
    node.vz = node.vz + nz * acc * dt
  end
  set_flame(node, throttle > 0.02, throttle)

  -- ---- write the node's orientation (nose = +Y, fwd = −Z) -----------------
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

  -- ---- HUD (10 Hz) --------------------------------------------------------
  if time - hud_t >= 0.1 then
    hud_t = time
    local dom = space.dominant(node.x, node.y, node.z)
    local b = dom and space.body(dom)
    local lines = {}
    local bars = math.floor(throttle * 10 + 0.5)
    lines[1] = string.format("THR [%s%s] %3d%%   SAS %s%s",
      string.rep("|", bars), string.rep("·", 10 - bars), throttle * 100,
      sas and "ON " or "off",
      node.grounded and "   LANDED" or "")
    if b then
      local dxr, dyr, dzr = node.x - b.x, node.y - b.y, node.z - b.z
      local rlen = math.sqrt(dxr * dxr + dyr * dyr + dzr * dzr)
      local upx, upy, upz = dxr / rlen, dyr / rlen, dzr / rlen
      local vsp = node.vx * upx + node.vy * upy + node.vz * upz
      local pitch_deg = math.deg(math.asin(math.max(-1, math.min(1,
        nx * upx + ny * upy + nz * upz)))) -- 90 = nose straight up
      lines[2] = string.format("ALT %6.0f   SPD %6.1f   VSPD %+6.1f   NOSE %+3.0f°",
        rlen - b.radius, spd, vsp, pitch_deg)
      -- THE orbit-insertion instrument: your speed vs circular-orbit speed vs
      -- escape speed AT THIS RADIUS. Stable orbit = hold SPD near "orb" with
      -- VSPD ~ 0; past "esc" you are leaving, however it feels.
      local vorb = math.sqrt(b.mu / rlen)
      local vesc = vorb * 1.41421
      local tag = ""
      if spd >= vesc then tag = "  ▲▲ ESCAPING"
      elseif spd >= vesc * 0.93 then tag = "  ▲ near escape" end
      lines[3] = string.format("V-ORBIT %5.1f   V-ESC %5.1f%s", vorb, vesc, tag)
      local o = space.elements(node.x, node.y, node.z, node.vx, node.vy, node.vz)
      if o and o.apoapsis then
        lines[4] = string.format("ORBIT [%s]  pe %+.0f  ap %+.0f  T %.0fs",
          o.body, o.periapsis - b.radius, o.apoapsis - b.radius, o.period)
      elseif o then
        lines[4] = string.format("ESCAPE [%s]  pe %+.0f", o.body, o.periapsis - b.radius)
      end
    end
    lines[#lines + 1] =
      "F exit · Shift/Ctrl thr · X cut · Z full · WASD/QE rotate · T SAS"
    set_hud(node, table.concat(lines, "\n"))
  end

  pvx, pvy, pvz = node.vx, node.vy, node.vz
end

-- Crashes judged by the PRE-impact velocity (see header). Grace after spawns.
function onCollisionEnter(node, other, hit)
  if wrecked or not piloting then return end
  if time - spawn_t < params.grace then return end
  -- The parked astronaut rides INSIDE the hull: its body-overlap events are
  -- not impacts. (Body-body events have no solver response, and the pair can
  -- re-fire mid-flight — at orbital speed that read as a phantom crash.)
  if astronaut and other.id == astronaut.id then return end
  -- Impact speed is RELATIVE: subtract the other node's velocity when it has
  -- one (a body). Terrain has none — absolute is correct there.
  local ovx, ovy, ovz = other.vx or 0, other.vy or 0, other.vz or 0
  local vn = math.abs(
    (pvx - ovx) * hit.nx + (pvy - ovy) * hit.ny + (pvz - ovz) * hit.nz)
  if vn > params.crash_speed then
    wrecked = true
    throttle = 0.0
    set_flame(node, false, 0)
    spawnEffect("Explosion", hit.x, hit.y, hit.z)
    print(string.format("CRASH at %.1f m/s — ship wrecked (G to restore)", vn))
  end
end
