-- The ship (solar demo S3 v2): fly like KSP — rate-commanded attitude, real
-- F = ma under µ/r² gravity, a parking brake, honest crash detection, an
-- engine plume that follows the throttle, and a live HUD.
--
--   F        board / exit (walk within `board_range`)
--   SHIFT / CTRL   throttle up / down        X  cut throttle       Z  full
--   W/S      pitch (S pulls the nose UP)    A/D  yaw    Q/E  roll
--            (hold = turn, release = stop)
--   B        landing gear (legs retracted = fragile belly, crash at 6 m/s)
--   T        SAS toggle       G  (while wrecked) restore at the pad
--
-- Fuel is real: burning scales with throttle, the ship gets LIGHTER as the
-- tank drains (thrust/mass — TWR climbs, watch the G limit near empty), an
-- empty tank means no thrust, and the pad refuels a parked ship. A wreck now
-- actually falls apart: the hull vanishes into an explosion and a shower of
-- tumbling debris bodies that rain back onto the terrain (G cleans them up).
--   M        map screen (orbit view; ↑/↓ zoom while open)
--   . / ,    time-warp up / down (KSP rules: only while coasting or parked;
--            any control input drops warp back to 1×; the engine snaps a
--            coasting ship to exact Kepler rails, so high warp is drift-free
--            and auto-cancels on surface proximity)
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
  mass = 2.0,           -- tonnes-ish WET mass; accel = thrust / current mass
  fuel = 100.0,         -- tank capacity
  burn_rate = 1.1,      -- units/s at full throttle (~90 s of full burn)
  fuel_mass = 0.6,      -- how much of `mass` is fuel at a full tank
  refuel_rate = 15.0,   -- units/s while parked at the pad
  max_thrust = 44.0,    -- max accel 22 vs surface g 9.8 → TWR ≈ 2.2 (wet)
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
fuel = 100.0

local sas = true
-- Time-warp ladder (KSP-style steps) + a short HUD notice when a step is denied.
local warp_steps = { 1, 5, 10, 50, 100, 1000, 10000 }
local warp_note, warp_note_t = nil, -10.0
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
  fuel = params.fuel
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

-- ---- landing legs -----------------------------------------------------------
-- Four strut children of the hull, authored DEPLOYED in the scene; we cache
-- their authored local transforms on first sight and animate toward/away from
-- them (retracted = tucked up beside the hull). Gear down is what makes a
-- touchdown survivable — a bare belly wrecks at 6 m/s instead of crash_speed.
local legs, legs_deployed, leg_anim = nil, true, 1.0
local function find_legs()
  if legs then return end
  legs = {}
  for _, nm in ipairs({ "Leg A", "Leg B", "Leg C", "Leg D" }) do
    local l = find(nm)
    if l then
      legs[#legs + 1] = { node = l, x = l.x, y = l.y, z = l.z, sy = l.scale_y }
    end
  end
end

local function animate_legs(dt)
  find_legs()
  local f = leg_anim
  for _, l in ipairs(legs) do
    l.node.x = l.x * (0.55 + 0.45 * f)
    l.node.z = l.z * (0.55 + 0.45 * f)
    l.node.y = l.y * f + 0.12 * (1 - f)
    l.node.scale_y = l.sy * (0.3 + 0.7 * f)
  end
end

local function set_ship_visible(node, on)
  node.visible = on
  find_legs()
  for _, l in ipairs(legs) do l.node.visible = on end
end

-- ---- wreckage ---------------------------------------------------------------
local debris = {}
local function scatter_debris(node)
  for i = 1, 7 do
    local a = i * 0.897 + time
    local sp = 4 + (i % 3) * 3
    spawn("Debris", vec3(node.x, node.y + 0.5, node.z), function(d)
      d.vx = node.vx + math.cos(a) * sp
      d.vy = node.vy + 3 + (i % 2) * 4
      d.vz = node.vz + math.sin(a) * sp
      d.scale_x = 0.5 + 0.16 * (i % 4)
      d.scale_y = 0.3 + 0.11 * ((i + 1) % 3)
      d.scale_z = 0.4 + 0.14 * ((i + 2) % 3)
      debris[#debris + 1] = d
    end)
  end
end

local function wreck_ship(node, x, y, z)
  wrecked = true
  throttle = 0.0
  set_flame(node, false, 0)
  spawnEffect("Explosion", x, y, z)
  scatter_debris(node)
  set_ship_visible(node, false)
end

-- The navball + the G5-style flight instruments flanking it (speed tape left,
-- altitude tape right, heading readout above — the pilot's layout).
local navball, tape_spd, tape_alt, txt_spd, txt_alt, txt_hdg
-- Published for the HUD blocks: the current compass heading in degrees.
local heading_deg = 0
local function find_instruments()
  if navball then return end
  navball = find("Navball")
  tape_spd, tape_alt = find("Speed Tape"), find("Alt Tape")
  txt_spd, txt_alt = find("Speed Readout"), find("Alt Readout")
  txt_hdg = find("Heading Readout")
end

local function set_navball(on)
  find_instruments()
  for _, inst in ipairs({ navball, tape_spd, tape_alt, txt_spd, txt_alt, txt_hdg }) do
    if inst then
      local el = inst:getcomponent("UiElement")
      if el then el.visible = on and 1 or 0 end
    end
  end
end

-- Feed the navball shader: the ship's basis expressed in the LOCAL HORIZON
-- frame (x = east, y = radial up, z = north) + the prograde direction. The
-- ball is drawn entirely by shaders/navball.flsl — these are its uniforms.
-- The east reference comes from world-Y — except near the poles, where
-- cross(Y, up) degenerates and the heading would swim with every position
-- change (the pilot's "heading changes when I only pitch" report — the pad
-- IS at the north pole). There we anchor east to world-Z instead.
local function update_navball(node)
  find_instruments()
  if not navball then return end
  local d = space.dominant(node.x, node.y, node.z)
  local b = d and space.body(d)
  if not b then return end
  local ux, uy, uz = norm(node.x - b.x, node.y - b.y, node.z - b.z)
  local ex, ey, ez
  if math.abs(uy) > 0.93 then
    ex, ey, ez = norm(cross(0, 0, 1, ux, uy, uz))
  else
    ex, ey, ez = norm(cross(0, 1, 0, ux, uy, uz))
  end
  if ex == 0 and ey == 0 and ez == 0 then ex, ey, ez = 1, 0, 0 end
  local nhx, nhy, nhz = cross(ux, uy, uz, ex, ey, ez)
  local function toH(vx, vy, vz)
    return vx * ex + vy * ey + vz * ez,
           vx * ux + vy * uy + vz * uz,
           vx * nhx + vy * nhy + vz * nhz
  end
  local rx2, ry2, rz2 = cross(fx, fy, fz, nx, ny, nz)
  local bx2, by2, bz2 = cross(nx, ny, nz, rx2, ry2, rz2)
  navball:setShaderParam("right", toH(rx2, ry2, rz2))
  navball:setShaderParam("up", toH(bx2, by2, bz2))
  navball:setShaderParam("nose", toH(nx, ny, nz))
  local vl = math.sqrt(node.vx ^ 2 + node.vy ^ 2 + node.vz ^ 2)
  if vl > 2.0 then
    navball:setShaderParam("prograde", toH(node.vx / vl, node.vy / vl, node.vz / vl))
  else
    navball:setShaderParam("prograde", 0, 0, 0)
  end
  -- Compass heading of the nose's horizontal projection (0 = north, 90 = east).
  local he = nx * ex + ny * ey + nz * ez
  local hn = nx * nhx + ny * nhy + nz * nhz
  heading_deg = (math.deg(math.atan2(he, hn)) + 360) % 360
  -- G5 tapes: speed on the left, altitude on the right, value windows + HDG.
  local dxr, dyr, dzr = node.x - b.x, node.y - b.y, node.z - b.z
  local alt = math.sqrt(dxr * dxr + dyr * dyr + dzr * dzr) - b.radius
  if tape_spd then tape_spd:setShaderParam("tape", vl, 40, 5) end
  if tape_alt then tape_alt:setShaderParam("tape", alt, 150, 25) end
  if txt_spd then txt_spd.text = string.format("%.0f", vl) end
  if txt_alt then txt_alt.text = string.format("%.0f", alt) end
  if txt_hdg then txt_hdg.text = string.format("HDG %03.0f°", heading_deg) end
end

-- ---- the map screen (S6) ----------------------------------------------------
-- Everything is drawn by shaders/map.flsl; this side only does the orbital
-- mechanics. We project the world onto the ship's ORBITAL PLANE (normal = the
-- angular momentum r × v) with periapsis along +X — in that frame the conic
-- r = p/(1+e·cosθ) is exactly `len(q) + e·q.x = p`, which the shader draws
-- with zero trig. The dominant body sits at the panel center; the sibling
-- body (moon ↔ planet), its orbit ring and its SOI are projected in too, so
-- a transfer is planned by eyeballing your conic against the moon's SOI ring.
local map_node, map_on, map_span = nil, false, nil
local function set_map(on)
  if not map_node then map_node = find("Map") end
  if not map_node then return end
  local el = map_node:getcomponent("UiElement")
  if el then el.visible = on and 1 or 0 end
end

local function update_map(node, dt)
  if not map_on or not map_node then return end
  local dn = space.dominant(node.x, node.y, node.z)
  local b = dn and space.body(dn)
  if not b then return end
  local rx, ry, rz = node.x - b.x, node.y - b.y, node.z - b.z
  local ux, uy, uz = node.vx - b.vx, node.vy - b.vy, node.vz - b.vz
  local rlen = math.sqrt(rx * rx + ry * ry + rz * rz)
  local hx, hy, hz = cross(rx, ry, rz, ux, uy, uz)
  local h2 = hx * hx + hy * hy + hz * hz
  local has_orbit = h2 > 1e-3 and rlen > 1e-3
  -- In-plane basis: ê1 = periapsis direction, ê2 = ĥ × ê1 (motion counter-
  -- clockwise on screen). Parked/degenerate → radial +X so the ship still shows.
  local e1x, e1y, e1z, e2x, e2y, e2z = 1, 0, 0, 0, 0, 1
  local p, ecc = 0, 0
  if has_orbit then
    p = h2 / b.mu
    local cx2, cy2, cz2 = cross(ux, uy, uz, hx, hy, hz)
    local evx = cx2 / b.mu - rx / rlen
    local evy = cy2 / b.mu - ry / rlen
    local evz = cz2 / b.mu - rz / rlen
    ecc = math.sqrt(evx * evx + evy * evy + evz * evz)
    if ecc > 1e-5 then
      e1x, e1y, e1z = evx / ecc, evy / ecc, evz / ecc
    else
      e1x, e1y, e1z = rx / rlen, ry / rlen, rz / rlen
    end
    local hl = math.sqrt(h2)
    e2x, e2y, e2z = cross(hx / hl, hy / hl, hz / hl, e1x, e1y, e1z)
  elseif rlen > 1e-3 then
    e1x, e1y, e1z = rx / rlen, ry / rlen, rz / rlen
    e2x, e2y, e2z = norm(cross(e1x, e1y, e1z, 0, 1, 0))
    if e2x == 0 and e2y == 0 and e2z == 0 then e2x, e2y, e2z = 0, 0, 1 end
  end
  local sx = rx * e1x + ry * e1y + rz * e1z
  local sy = rx * e2x + ry * e2y + rz * e2z
  local vlen = math.sqrt(ux * ux + uy * uy + uz * uz)
  local vmx, vmy = 0, 0
  if vlen > 0.5 then
    vmx = (ux * e1x + uy * e1y + uz * e1z) / vlen
    vmy = (ux * e2x + uy * e2y + uz * e2z) / vlen
  end
  -- The nearest sibling body, projected into the same plane.
  local ox, oy, orad, osoi, orbr, opres = 0, 0, 0, 0, 0, 0
  for _, o in ipairs(space.bodies()) do
    if o.name ~= b.name then
      local dx2, dy2, dz2 = o.x - b.x, o.y - b.y, o.z - b.z
      local dd = math.sqrt(dx2 * dx2 + dy2 * dy2 + dz2 * dz2)
      if opres == 0 or dd < orbr then
        ox = dx2 * e1x + dy2 * e1y + dz2 * e1z
        oy = dx2 * e2x + dy2 * e2y + dz2 * e2z
        orad, orbr, opres = o.radius, dd, 1
        osoi = o.soi > 0 and o.soi or 0
      end
    end
  end
  -- Auto-fit the view to the current conic on open; ↑/↓ zoom afterwards.
  if not map_span then
    map_span = math.max(rlen * 1.4, b.radius * 3.0)
    if has_orbit and ecc < 1 then
      map_span = math.max(map_span, p / (1 - ecc) * 1.25)
    end
  end
  if input.key("up") then map_span = map_span * (1 - 1.6 * dt) end
  if input.key("down") then map_span = map_span * (1 + 1.6 * dt) end
  map_span = math.max(b.radius * 1.2, math.min(map_span, 500000))
  map_node:setShaderParam("view", map_span, has_orbit and 1 or 0, vlen > 0.5 and 1 or 0)
  map_node:setShaderParam("conic", p, ecc, (has_orbit and ecc < 1) and 1 or 0)
  map_node:setShaderParam("shipm", sx, sy, 0)
  map_node:setShaderParam("velm", vmx, vmy, 0)
  map_node:setShaderParam("focusb", b.radius, b.soi > 0 and b.soi or -1, 0)
  map_node:setShaderParam("otherb", ox, oy, orad)
  map_node:setShaderParam("otherb2", osoi, orbr, opres)
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
      set_navball(false)
      map_on = false
      set_map(false)
    elseif distance(astronaut, node) <= params.board_range then
      piloting = true
      astronaut.visible = false
      set_navball(true)
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
    if space.warp() > 1.001 then space.warp(1) end -- a wreck flies realtime
    set_flame(node, false, 0)
    set_hud(node, "SHIP WRECKED — press G to restore at the pad, F to exit")
    if input.pressed("g") then
      wrecked = false
      node.x, node.y, node.z = pad_x, pad_y, pad_z + 0.0
      reset_pose(node)
      set_ship_visible(node, true)
      legs_deployed, leg_anim = true, 1.0
      animate_legs(0)
      for _, d in ipairs(debris) do destroy(d) end
      debris = {}
      print("ship restored at the pad")
    end
    pvx, pvy, pvz = node.vx, node.vy, node.vz
    return
  end

  if input.pressed("t") then sas = not sas end

  -- ---- map screen ----------------------------------------------------------
  if input.pressed("m") then
    map_on = not map_on
    map_span = nil -- re-fit to the current orbit every time it opens
    set_map(map_on)
  end

  -- ---- landing gear --------------------------------------------------------
  if input.pressed("b") then legs_deployed = not legs_deployed end
  leg_anim = toward(leg_anim, legs_deployed and 1 or 0, dt / 0.6)
  animate_legs(dt)

  -- ---- time warp -----------------------------------------------------------
  local warp = space.warp()
  local function warp_step(dir)
    local idx = 1
    for i, w in ipairs(warp_steps) do
      if warp >= w - 0.5 then idx = i end
    end
    local nxt = warp_steps[math.max(1, math.min(#warp_steps, idx + dir))]
    if nxt > warp then
      -- Stepping UP obeys the KSP rules: no thrust, and either parked on the
      -- ground or high enough that the conic can't clip terrain this instant.
      local alt_ok = node.grounded
      local d0 = space.dominant(node.x, node.y, node.z)
      local b0 = d0 and space.body(d0)
      if not alt_ok and b0 then
        local rr = math.sqrt((node.x - b0.x) ^ 2 + (node.y - b0.y) ^ 2 + (node.z - b0.z) ^ 2)
        alt_ok = rr - b0.radius > 40.0
      end
      if throttle > 0.01 then
        warp_note, warp_note_t = "warp locked: cut throttle first", time
        return
      elseif not alt_ok then
        warp_note, warp_note_t = "warp locked: too low", time
        return
      end
    end
    if nxt ~= warp then space.warp(nxt) end
  end
  if input.pressed(".") then warp_step(1) end
  if input.pressed(",") then warp_step(-1) end
  -- Any hands-on-stick input cancels warp (throttle keys, attitude keys) —
  -- you cannot fly the ship at 100×; the engine's rails own it up there.
  local control_touched = input.key("shift") or input.key("ctrl") or input.key("z")
    or input.key("w") or input.key("a") or input.key("s") or input.key("d")
    or input.key("q") or input.key("e")
  if warp > 1.001 then
    if control_touched then
      space.warp(1)
      warp_note, warp_note_t = "warp canceled — pilot input", time
    end
    -- Coasting on rails: HUD only; attitude/thrust/brake wait for realtime.
    set_flame(node, false, 0)
    if time - hud_t >= 0.1 then
      hud_t = time
      local dom = space.dominant(node.x, node.y, node.z)
      local b = dom and space.body(dom)
      local lines = {}
      lines[1] = string.format("WARP ×%d   (. faster · , slower · any control cancels)", warp)
      if b then
        local dxr, dyr, dzr = node.x - b.x, node.y - b.y, node.z - b.z
        local rlen = math.sqrt(dxr * dxr + dyr * dyr + dzr * dzr)
        lines[2] = string.format("ALT %6.0f  [%s]  t %.0fs", rlen - b.radius, dom, space.time())
        local o = space.elements(node.x, node.y, node.z, node.vx, node.vy, node.vz)
        if o and o.apoapsis then
          lines[3] = string.format("ORBIT  pe %+.0f  ap %+.0f  T %.0fs",
            o.periapsis - b.radius, o.apoapsis - b.radius, o.period)
        end
      end
      if warp_note and time - warp_note_t < 2.5 then lines[#lines + 1] = "⚠ " .. warp_note end
      set_hud(node, table.concat(lines, "\n"))
    end
    -- Parked warp (waiting out a transfer window on the pad): keep the brake
    -- pinned so the sphere hull can't slow-slide down a slope for game-days.
    if node.grounded then node.vx, node.vy, node.vz = 0, 0, 0 end
    update_navball(node)
    update_map(node, dt)
    pvx, pvy, pvz = node.vx, node.vy, node.vz
    return
  end

  -- ---- throttle + fuel -----------------------------------------------------
  if input.key("shift") then throttle = throttle + params.throttle_rate * dt end
  if input.key("ctrl") then throttle = throttle - params.throttle_rate * dt end
  if input.key("x") then throttle = 0.0 end
  if input.key("z") then throttle = 1.0 end
  if throttle > 1.0 then throttle = 1.0 end
  if throttle < 0.0 then throttle = 0.0 end
  if fuel <= 0.0 then throttle = 0.0 end
  fuel = math.max(0.0, fuel - throttle * params.burn_rate * dt)
  -- The pad refuels a parked ship (grounded, engine idle, near the spawn).
  local refueling = false
  if node.grounded and throttle < 0.01 and fuel < params.fuel then
    local pd = math.sqrt((node.x - pad_x) ^ 2 + (node.y - pad_y) ^ 2 + (node.z - pad_z) ^ 2)
    if pd < 40.0 then
      fuel = math.min(params.fuel, fuel + params.refuel_rate * dt)
      refueling = true
    end
  end

  -- ---- attitude: RATE-COMMANDED (the KSP feel) ----------------------------
  -- Stick convention (per Brody, the resident pilot): PULL BACK (S) pitches
  -- the nose UP, push forward (W) pitches it DOWN.
  local p = (input.key("w") and 1 or 0) - (input.key("s") and 1 or 0)
  local y = (input.key("a") and 1 or 0) - (input.key("d") and 1 or 0)
  local r = (input.key("e") and 1 or 0) - (input.key("q") and 1 or 0)
  local step = params.rate_accel * dt
  -- SAS off = rates persist when released (pure Newton, for the purists).
  local hold = sas and 0 or nil
  avp = toward(avp, p ~= 0 and p * params.max_rate or (hold or avp), step)
  avy = toward(avy, y ~= 0 and y * params.max_rate or (hold or avy), step)
  avr = toward(avr, r ~= 0 and r * params.max_rate or (hold or avr), step)

  -- Axis wiring (the pilot's fix): for a rocket whose long axis is `nose`,
  -- PITCH tilts about `right`, YAW tilts the nose left/right about `fwd`
  -- (the belly axis), and ROLL spins about `nose` itself. (These were
  -- swapped — A/D used to spin the hull, which read as heading drift.)
  local rx2, ry2, rz2 = cross(fx, fy, fz, nx, ny, nz) -- ship right
  local wx = rx2 * avp + fx * avy + nx * avr
  local wy = ry2 * avp + fy * avy + ny * avr
  local wz = rz2 * avp + fz * avy + nz * avr
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
  -- The ship gets lighter as fuel burns; near-empty at full throttle can
  -- exceed the frame's G limit — the KSP "throttle down when light" rule.
  local mass = params.mass - params.fuel_mass * (1.0 - fuel / params.fuel)
  local acc = throttle * params.max_thrust / mass
  if acc > params.max_g then
    wreck_ship(node, node.x, node.y, node.z)
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

  update_navball(node)
  update_map(node, dt)

  -- ---- HUD (10 Hz) --------------------------------------------------------
  if time - hud_t >= 0.1 then
    hud_t = time
    local dom = space.dominant(node.x, node.y, node.z)
    local b = dom and space.body(dom)
    local lines = {}
    local bars = math.floor(throttle * 10 + 0.5)
    lines[1] = string.format("THR [%s%s] %3d%%   SAS %s   GEAR %s%s",
      string.rep("|", bars), string.rep("·", 10 - bars), throttle * 100,
      sas and "ON " or "off",
      legs_deployed and "▼" or "▲",
      node.grounded and "   LANDED" or "")
    local fbars = math.floor(fuel / params.fuel * 10 + 0.5)
    local ftag = ""
    if refueling then ftag = "  REFUELING"
    elseif fuel <= 0.0 then ftag = "  ⚠ TANK EMPTY"
    elseif acc > params.max_g * 0.88 and throttle > 0.01 then ftag = "  ⚠ NEAR G LIMIT" end
    lines[2] = string.format("FUEL [%s%s] %3d%%%s",
      string.rep("|", fbars), string.rep("·", 10 - fbars), fuel / params.fuel * 100, ftag)
    if b then
      local dxr, dyr, dzr = node.x - b.x, node.y - b.y, node.z - b.z
      local rlen = math.sqrt(dxr * dxr + dyr * dyr + dzr * dzr)
      local upx, upy, upz = dxr / rlen, dyr / rlen, dzr / rlen
      local vsp = node.vx * upx + node.vy * upy + node.vz * upz
      local pitch_deg = math.deg(math.asin(math.max(-1, math.min(1,
        nx * upx + ny * upy + nz * upz)))) -- 90 = nose straight up
      lines[#lines + 1] = string.format("ALT %6.0f   SPD %6.1f   VSPD %+6.1f   NOSE %+3.0f°",
        rlen - b.radius, spd, vsp, pitch_deg)
      -- THE orbit-insertion instrument: your speed vs circular-orbit speed vs
      -- escape speed AT THIS RADIUS. Stable orbit = hold SPD near "orb" with
      -- VSPD ~ 0; past "esc" you are leaving, however it feels.
      local vorb = math.sqrt(b.mu / rlen)
      local vesc = vorb * 1.41421
      local tag = ""
      if spd >= vesc then tag = "  ▲▲ ESCAPING"
      elseif spd >= vesc * 0.93 then tag = "  ▲ near escape" end
      lines[#lines + 1] = string.format("V-ORBIT %5.1f   V-ESC %5.1f%s", vorb, vesc, tag)
      local o = space.elements(node.x, node.y, node.z, node.vx, node.vy, node.vz)
      if o and o.apoapsis then
        lines[#lines + 1] = string.format("ORBIT [%s]  pe %+.0f  ap %+.0f  T %.0fs",
          o.body, o.periapsis - b.radius, o.apoapsis - b.radius, o.period)
      elseif o then
        lines[#lines + 1] = string.format("ESCAPE [%s]  pe %+.0f", o.body, o.periapsis - b.radius)
      end
    end
    if warp_note and time - warp_note_t < 2.5 then lines[#lines + 1] = "⚠ " .. warp_note end
    lines[#lines + 1] =
      "F exit · Shift/Ctrl thr · X cut · Z full · WASD/QE rotate · T SAS · B gear · ./, warp · M map"
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
  -- Gear down absorbs a real landing; a bare belly is fragile.
  local limit = leg_anim > 0.8 and params.crash_speed or 6.0
  if vn > limit then
    wreck_ship(node, hit.x, hit.y, hit.z)
    print(string.format("CRASH at %.1f m/s%s — ship wrecked (G to restore)", vn,
      leg_anim > 0.8 and "" or " (gear was up!)"))
  end
end
