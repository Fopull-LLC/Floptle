-- The ship (solar demo S3 v2): fly like KSP — rate-commanded attitude, real
-- F = ma under µ/r² gravity, a parking brake, honest crash detection, an
-- engine plume that follows the throttle, and a live HUD.
--
--   F        board / exit (walk within `board_range`)
--   SHIFT / CTRL   throttle up / down        X  cut throttle       Z  full
--   W/S      pitch (S pulls the nose UP)    A/D  yaw    Q/E  roll
--            (hold = turn, release = stop)
--   B        landing gear (legs retracted = fragile belly, crash at 6 m/s)
--   T        SAS on/off    1-8  hold mode: 1 stability · 2 prograde ·
--            3 retrograde · 4 normal · 5 anti-nml · 6 radial-in · 7 radial-out ·
--            8 node (auto-point at the burn). G  (wrecked) restore at the pad
--
-- Fuel is real: burning scales with throttle, the ship gets LIGHTER as the
-- tank drains (thrust/mass — TWR climbs, watch the G limit near empty), an
-- empty tank means no thrust, and the pad refuels a parked ship. A wreck now
-- actually falls apart: the hull vanishes into an explosion and a shower of
-- tumbling debris bodies that rain back onto the terrain (G cleans them up).
--   M        map screen (orbit view; RMB-drag orbit, ↑/↓ zoom, TAB focus)
--            N   plan a maneuver node — then W/S prograde, A/D radial,
--                Q/E normal tune the burn, ←/→ slide it along the orbit, X
--                zeroes it. The map projects the resulting orbit and marks
--                where it drops you into another body's SOI (the encounter).
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

-- SAS autopilot (KSP hold modes). "off" = free (rates persist), "stability" =
-- damp rotation to zero, and the pointing modes auto-rotate the nose to that
-- direction with the rate controller. T toggles off/stability; number keys pick
-- a mode in flight (1..8). `sas_last` remembers the mode T re-enables. sas_mode
-- is a PUBLISHED global so the HUD's SAS buttons (sas_button.lua) can read the
-- active mode (highlight) and call setSAS() to change it.
sas_mode = "stability"
local sas_last = "stability"
-- Time-warp ladder (KSP-style steps) + a short HUD notice when a step is denied.
local warp_steps = { 1, 5, 10, 50, 100, 1000, 10000 }
local warp_note, warp_note_t = nil, -10.0
-- Ship basis (world space): nose = thrust axis; fwd/right complete the frame.
local nx, ny, nz = 0.0, 1.0, 0.0
local fx, fy, fz = 0.0, 0.0, -1.0
local avp, avy, avr = 0.0, 0.0, 0.0 -- angular rates about right/nose/fwd
-- Landed toppling (inverted pendulum on the gear footprint): a landed ship can't
-- freely spin — pilot pitch/yaw LEAN it, and gravity either rights it (gear down =
-- wide, stable) or tips it past balance into a topple (gear up = narrow). `tip_w`
-- is the lean rate, `toppled` latches a committed fall, `grounded_until` debounces
-- the flickery per-contact grounded flag so the model doesn't chatter.
local tip_w, toppled, grounded_until = 0.0, false, -10.0
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
  tip_w, toppled = 0.0, false
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

-- Landed attitude = an inverted pendulum on the gear footprint. The nose leans
-- from local-up by `theta`; gravity torque about the footprint edge is RESTORING
-- while the centre of mass sits over the base (tan θ < r/h) and RUNAWAY past it —
-- so a WIDE base (gear down) self-rights and resists the pilot, a NARROW base
-- (gear up) tips from a nudge. Pilot pitch/yaw push the lean; roll is ignored
-- (no pirouetting on the legs). Rewrites the nose/fwd basis directly.
local COM_H, FOOT_UP, FOOT_DOWN, TIP_DAMP, PUSH_GAIN = 2.0, 0.35, 1.7, 4.0, 1.5
local function apply_topple(node, dt, p, y)
  -- Local up: away from gravity (fallback: radial from the dominant body).
  local gx, gy, gz = space.gravity(node.x, node.y, node.z)
  local gl = math.sqrt(gx * gx + gy * gy + gz * gz)
  local ux, uy, uz
  if gl > 1e-4 then
    ux, uy, uz = -gx / gl, -gy / gl, -gz / gl
  else
    local dd = space.dominant(node.x, node.y, node.z)
    local b = dd and space.body(dd)
    if b then ux, uy, uz = norm(node.x - b.x, node.y - b.y, node.z - b.z) else ux, uy, uz = 0, 1, 0 end
    gl = 9.8
  end
  local theta = math.acos(math.max(-1, math.min(1, nx * ux + ny * uy + nz * uz)))
  local r = FOOT_UP + (FOOT_DOWN - FOOT_UP) * leg_anim -- gear widens the base
  local theta_tip = math.atan(r / COM_H)
  -- Lean axis: perpendicular to up & nose. Near upright it's degenerate, so take
  -- it from the pilot's push (pitch tips about ship-right, yaw about ship-fwd).
  local rgx, rgy, rgz = cross(fx, fy, fz, nx, ny, nz) -- ship right
  local lax, lay, laz = cross(ux, uy, uz, nx, ny, nz)
  local ll = math.sqrt(lax * lax + lay * lay + laz * laz)
  local pushmag = math.sqrt(p * p + y * y)
  if ll < 1e-3 then
    local dx, dy, dz = rgx * p + fx * y, rgy * p + fy * y, rgz * p + fz * y
    local d2 = dx * ux + dy * uy + dz * uz
    lax, lay, laz = norm(dx - ux * d2, dy - uy * d2, dz - uz * d2)
    if lax == 0 and lay == 0 and laz == 0 then lax, lay, laz = norm(rgx, rgy, rgz) end
  else
    lax, lay, laz = lax / ll, lay / ll, laz / ll
  end
  -- Torque = gravity (signed by the balance) + the pilot's steady push.
  local grav_alpha = (gl / COM_H) * (math.sin(theta) - (r / COM_H) * math.cos(theta))
  tip_w = tip_w + (grav_alpha + PUSH_GAIN * pushmag) * dt
  if not toppled and theta < theta_tip then
    tip_w = tip_w - TIP_DAMP * tip_w * dt -- damp small wobbles back to upright
  end
  theta = math.max(0.0, theta + tip_w * dt)
  if theta > theta_tip * 1.2 then toppled = true end
  -- Rebuild the nose by leaning `up` about the lean axis; keep heading by
  -- projecting the old fwd onto the plane perpendicular to the new nose.
  nx, ny, nz = rot(ux, uy, uz, lax, lay, laz, theta)
  local du = fx * nx + fy * ny + fz * nz
  fx, fy, fz = norm(fx - nx * du, fy - ny * du, fz - nz * du)
  if fx == 0 and fy == 0 and fz == 0 then fx, fy, fz = norm(cross(nx, ny, nz, ux, uy, uz)) end
  -- A committed gear-UP topple that slams flat wrecks the ship (gear down = it
  -- just lies over and survives — that's what the legs buy you).
  if toppled and theta > 1.4 and leg_anim < 0.8 and time - spawn_t > params.grace then
    wreck_ship(node, node.x, node.y, node.z)
  end
end

-- The navball + the G5-style flight instruments flanking it (speed tape left,
-- altitude tape right, heading readout above — the pilot's layout).
local navball, tape_spd, tape_alt, txt_spd, txt_alt, txt_hdg, landing_cam
-- Published for the HUD blocks: the current compass heading in degrees.
local heading_deg = 0
local function find_instruments()
  if navball then return end
  navball = find("Navball")
  tape_spd, tape_alt = find("Speed Tape"), find("Alt Tape")
  txt_spd, txt_alt = find("Speed Readout"), find("Alt Readout")
  txt_hdg = find("Heading Readout")
  landing_cam = find("Landing Screen") -- the belly-cam feed (A1 render target)
end

local function set_navball(on)
  find_instruments()
  for _, inst in ipairs({ navball, tape_spd, tape_alt, txt_spd, txt_alt, txt_hdg, landing_cam }) do
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
local function update_navball(node, tgtx, tgty, tgtz)
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
  -- The SAS autopilot's aim point (green ring), or hidden when there's none.
  if tgtx then
    navball:setShaderParam("sasTarget", toH(tgtx, tgty, tgtz))
  else
    navball:setShaderParam("sasTarget", 0, 0, 0)
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

-- ---- the map (S6 v2): a REAL 3D interactive map -----------------------------
-- M toggles KSP-style map mode, with BLENDER-style navigation (Ty's spec):
-- opens focused on the SHIP; the mouse does nothing until you hold RIGHT
-- CLICK — then drag orbits around the selection — and CTRL+RIGHT-drag PANS
-- the camera offset relative to it, so you're never trapped on the focus.
-- Scroll / ↑↓ zooms, TAB cycles focus (ship first, then every body). The
-- engine line layer draws live orbit conics for everything — each body's
-- ellipse around its SOI-inferred parent, SOI rings, and the ship's own conic
-- with Pe/Ap markers — occluded naturally by the bodies themselves.
-- Indicator toggles while open: 1 orbits · 2 SOIs · 3 markers.
map_view = false -- published: planet_camera stands down while this is true
local map_focus, map_zoom = 1, nil -- focus 1 = THE SHIP, 2.. = bodies
local map_hud_t = -10.0
local map_yaw2, map_pitch2 = 0.6, 0.45
local map_offx, map_offy, map_offz = 0.0, 0.0, 0.0 -- CTRL-drag pan offset
local map_show = { orbits = true, soi = true, markers = true }
local cam_node

-- Maneuver node (KSP planning): a single planned burn at `t` seconds ahead of
-- now, split into prograde / normal / radial ΔV. Nil until you press N in the
-- map. Editing it re-projects the resulting orbit and re-walks the patched
-- conic for SOI encounters, all game-side on top of `space.propagate`.
local mnv = nil -- { t, pro, nor, rad }
-- The patched-conic walk is heavy (hundreds of two-body evals across SOI
-- changes), so it's recomputed at ~8 Hz and the resulting world polyline +
-- encounter markers are cached here and redrawn every frame (draw.line is
-- immediate mode). `traj_now` = the path you're on; `traj_mnv` = the post-burn
-- path (nil unless a node exists).
local traj_t = -10.0
local traj_now, traj_mnv = nil, nil

-- Orbital-plane basis + conic from a relative state vector: returns
-- has_orbit, p, ecc, ê1 (periapsis dir), ê2 (in-plane, motion side).
local function orbit_basis(rx, ry, rz, vx, vy, vz, mu)
  local rlen = math.sqrt(rx * rx + ry * ry + rz * rz)
  local hx, hy, hz = cross(rx, ry, rz, vx, vy, vz)
  local h2 = hx * hx + hy * hy + hz * hz
  if h2 < 1e-3 or rlen < 1e-3 then
    return false, 0, 0, 1, 0, 0, 0, 0, 1
  end
  local p = h2 / mu
  local cx2, cy2, cz2 = cross(vx, vy, vz, hx, hy, hz)
  local evx = cx2 / mu - rx / rlen
  local evy = cy2 / mu - ry / rlen
  local evz = cz2 / mu - rz / rlen
  local ecc = math.sqrt(evx * evx + evy * evy + evz * evz)
  local e1x, e1y, e1z
  if ecc > 1e-5 then
    e1x, e1y, e1z = evx / ecc, evy / ecc, evz / ecc
  else
    e1x, e1y, e1z = rx / rlen, ry / rlen, rz / rlen
  end
  local hl = math.sqrt(h2)
  local e2x, e2y, e2z = cross(hx / hl, hy / hl, hz / hl, e1x, e1y, e1z)
  return true, p, ecc, e1x, e1y, e1z, e2x, e2y, e2z
end

-- Draw r = p/(1+e·cosθ) around a world center in the ê1/ê2 plane. Handles
-- ellipses AND hyperbolas (invalid θ range just breaks the polyline).
local function draw_conic(cx, cy, cz, e1x, e1y, e1z, e2x, e2y, e2z, p, ecc, r, g, b, a)
  local segs = 128
  local px2, py2, pz2, has_prev = 0, 0, 0, false
  for i = 0, segs do
    local th = (i / segs) * 2 * math.pi
    local den = 1 + ecc * math.cos(th)
    if den > 0.02 then
      local rr = p / den
      local ct, st = math.cos(th), math.sin(th)
      local wx = cx + (e1x * ct + e2x * st) * rr
      local wy = cy + (e1y * ct + e2y * st) * rr
      local wz = cz + (e1z * ct + e2z * st) * rr
      if has_prev then draw.line(px2, py2, pz2, wx, wy, wz, r, g, b, a) end
      px2, py2, pz2, has_prev = wx, wy, wz, true
    else
      has_prev = false
    end
  end
end

local function draw_ring(cx, cy, cz, e1x, e1y, e1z, e2x, e2y, e2z, radius, r, g, b, a)
  draw_conic(cx, cy, cz, e1x, e1y, e1z, e2x, e2y, e2z, radius, 0, r, g, b, a)
end

local function draw_cross(x, y, z, s, r, g, b)
  draw.line(x - s, y, z, x + s, y, z, r, g, b, 1)
  draw.line(x, y - s, z, x, y + s, z, r, g, b, 1)
  draw.line(x, y, z - s, x, y, z + s, r, g, b, 1)
end

-- The parent of body i: the OTHER body with the smallest SOI still containing
-- it (patched conics — same rule the engine uses for dominance).
local function body_parent(bodies, i)
  local b = bodies[i]
  local best, bs = nil, nil
  for j, o in ipairs(bodies) do
    if j ~= i then
      local dx, dy, dz = b.x - o.x, b.y - o.y, b.z - o.z
      local d = math.sqrt(dx * dx + dy * dy + dz * dz)
      local soi = o.soi < 0 and math.huge or o.soi
      if d <= soi and (bs == nil or soi < bs) then best, bs = o, soi end
    end
  end
  return best
end

-- Parent INDEX of every body (the smallest SOI still containing it), so the
-- walkers can climb/descend the hierarchy by index without re-searching.
local function parent_indices(bodies)
  local pidx = {}
  for i, b in ipairs(bodies) do
    local best, bs = nil, nil
    for j, o in ipairs(bodies) do
      if j ~= i then
        local dx, dy, dz = b.x - o.x, b.y - o.y, b.z - o.z
        local d = math.sqrt(dx * dx + dy * dy + dz * dz)
        local soi = o.soi < 0 and math.huge or o.soi
        if d <= soi and (bs == nil or soi < bs) then best, bs = j, soi end
      end
    end
    pidx[i] = best
  end
  return pidx
end

-- World position AND velocity of body `i` at `tt` seconds from now — walk the
-- parent chain, propagating each link about its parent's µ and summing (a moon
-- rides its planet rides the star). The root is fixed. Exact, drift-free.
local function body_state_at(bodies, pidx, i, tt)
  local pi = pidx[i]
  local b = bodies[i]
  if not pi then return b.x, b.y, b.z, 0, 0, 0 end
  local px, py, pz, pvx, pvy, pvz = body_state_at(bodies, pidx, pi, tt)
  local par = bodies[pi]
  local rx, ry, rz, rvx, rvy, rvz = space.propagate(
    b.x - par.x, b.y - par.y, b.z - par.z,
    b.vx - par.vx, b.vy - par.vy, b.vz - par.vz, par.mu, tt)
  return px + rx, py + ry, pz + rz, pvx + rvx, pvy + rvy, pvz + rvz
end

-- A body's own orbital period around its parent (vis-viva), or nil if unbound
-- or a root. Sets the transfer timescale for an escape burn's walk.
local function body_period(bodies, pidx, i)
  local pi = pidx[i]
  if not pi then return nil end
  local b, par = bodies[i], bodies[pi]
  local rx, ry, rz = b.x - par.x, b.y - par.y, b.z - par.z
  local vx, vy, vz = b.vx - par.vx, b.vy - par.vy, b.vz - par.vz
  local rlen = math.sqrt(rx * rx + ry * ry + rz * rz)
  local v2 = vx * vx + vy * vy + vz * vz
  local a = 1.0 / (2.0 / rlen - v2 / par.mu)
  if a <= 0 then return nil end
  return 2 * math.pi * math.sqrt(a * a * a / par.mu)
end

-- Snapshot of every body's world state at `tt` seconds from now (one table so
-- the walk touches each body's parent chain once per step, not per test).
local function all_body_states(bodies, pidx, tt)
  local s = {}
  for i = 1, #bodies do
    local x, y, z, vx, vy, vz = body_state_at(bodies, pidx, i, tt)
    s[i] = { x = x, y = y, z = z, vx = vx, vy = vy, vz = vz }
  end
  return s
end

-- Walk a ship's future path as a real patched conic: advance the state within
-- the current dominant body's frame, and when it leaves that SOI drop to the
-- parent, or when it falls inside a child's SOI capture that child. Returns the
-- world-space polyline and the SOI-change / impact events found along it. This
-- is what turns "pretty orbit lines" into "you encounter Draol in 3m12s".
local function walk_trajectory(bodies, pidx, wx, wy, wz, wvx, wvy, wvz, cur, t0, span, segs)
  -- Points are stored RELATIVE TO their attractor + the attractor index, NOT in
  -- absolute world space. Drawn at attractor.LIVE + rel, so the whole path lives
  -- in the dominant body's REST FRAME (the KSP map): your orbit is a static
  -- ellipse around the planet's CURRENT position, not smeared along the planet's
  -- own orbit into wherever it will be in 180 s. That smear is what flung the
  -- node way out across the system.
  local rpts, anch, enc = {}, {}, {}
  local step = span / segs
  local tt = t0
  local start_states = all_body_states(bodies, pidx, tt)
  local s0 = start_states[cur]
  rpts[1], rpts[2], rpts[3] = wx - s0.x, wy - s0.y, wz - s0.z
  anch[1] = cur
  for _ = 1, segs do
    -- Ship relative to the current attractor at the START of this step.
    local cs = start_states[cur]
    local nrx, nry, nrz, nrvx, nrvy, nrvz = space.propagate(
      wx - cs.x, wy - cs.y, wz - cs.z, wvx - cs.vx, wvy - cs.vy, wvz - cs.vz,
      bodies[cur].mu, step)
    tt = tt + step
    -- One snapshot at the step END, reused for the recompose + every SOI test.
    local es = all_body_states(bodies, pidx, tt)
    local ec = es[cur]
    wx, wy, wz = ec.x + nrx, ec.y + nry, ec.z + nrz
    wvx, wvy, wvz = ec.vx + nrvx, ec.vy + nrvy, ec.vz + nrvz
    -- store REL to the current attractor (nr* IS wx-ec.x) + which body it is
    rpts[#rpts + 1] = nrx
    rpts[#rpts + 1] = nry
    rpts[#rpts + 1] = nrz
    anch[#anch + 1] = cur
    -- Capture a child SOI we've fallen into (deeper body wins).
    for j, o in ipairs(bodies) do
      if pidx[j] == cur and o.soi > 0 then
        local d = es[j]
        local dx, dy, dz = wx - d.x, wy - d.y, wz - d.z
        if dx * dx + dy * dy + dz * dz < o.soi * o.soi then
          enc[#enc + 1] = { t = tt, name = o.name, anchor = j, rx = dx, ry = dy, rz = dz, kind = "enter" }
          cur = j
          break
        end
      end
    end
    -- Or climb out of the current SOI into the parent's frame.
    local pc = pidx[cur]
    if pc and bodies[cur].soi > 0 then
      local d = es[cur]
      local dx, dy, dz = wx - d.x, wy - d.y, wz - d.z
      if dx * dx + dy * dy + dz * dz > bodies[cur].soi * bodies[cur].soi then
        enc[#enc + 1] = { t = tt, name = bodies[cur].name, anchor = cur, rx = dx, ry = dy, rz = dz, kind = "exit" }
        cur = pc
      end
    end
    -- Terrain impact ends the walk (the conic would dive underground).
    local sb = bodies[cur]
    local d = es[cur]
    local dx, dy, dz = wx - d.x, wy - d.y, wz - d.z
    if dx * dx + dy * dy + dz * dz < sb.radius * sb.radius then
      enc[#enc + 1] = { t = tt, name = sb.name, anchor = cur, rx = dx, ry = dy, rz = dz, kind = "impact" }
      break
    end
    start_states = es -- this step's END is the next step's START
  end
  return rpts, anch, enc
end

-- Walk the CURRENT patched conic forward to `t_target`, crossing every SOI in
-- between, and return the ship's state THERE — the dominant body it's under plus
-- its position AND velocity relative to that body (and the absolute world state).
-- THIS is what makes a maneuver node placed BEYOND an SOI crossing land on the
-- real path and apply its burn in the right frame: true across-SOI planning,
-- not a single two-body arc around the starting planet.
local function patched_state_at(bodies, pidx, wx, wy, wz, wvx, wvy, wvz, cur, t_start, t_target, steps)
  steps = math.max(1, steps)
  local step = (t_target - t_start) / steps
  local tt = t_start
  local start_states = all_body_states(bodies, pidx, tt)
  for _ = 1, steps do
    local cs = start_states[cur]
    local nrx, nry, nrz, nrvx, nrvy, nrvz = space.propagate(
      wx - cs.x, wy - cs.y, wz - cs.z, wvx - cs.vx, wvy - cs.vy, wvz - cs.vz,
      bodies[cur].mu, step)
    tt = tt + step
    local es = all_body_states(bodies, pidx, tt)
    local ec = es[cur]
    wx, wy, wz = ec.x + nrx, ec.y + nry, ec.z + nrz
    wvx, wvy, wvz = ec.vx + nrvx, ec.vy + nrvy, ec.vz + nrvz
    for j, o in ipairs(bodies) do -- fall into a child SOI
      if pidx[j] == cur and o.soi > 0 then
        local d = es[j]
        local dx, dy, dz = wx - d.x, wy - d.y, wz - d.z
        if dx * dx + dy * dy + dz * dz < o.soi * o.soi then cur = j break end
      end
    end
    local pc = pidx[cur] -- climb out to the parent
    if pc and bodies[cur].soi > 0 then
      local d = es[cur]
      local dx, dy, dz = wx - d.x, wy - d.y, wz - d.z
      if dx * dx + dy * dy + dz * dz > bodies[cur].soi * bodies[cur].soi then cur = pc end
    end
    start_states = es
  end
  local d = start_states[cur]
  return {
    anchor = cur, mu = bodies[cur].mu,
    wx = wx, wy = wy, wz = wz, wvx = wvx, wvy = wvy, wvz = wvz,
    rx = wx - d.x, ry = wy - d.y, rz = wz - d.z,
    rvx = wvx - d.vx, rvy = wvy - d.vy, rvz = wvz - d.vz,
  }
end

-- A stored (anchor, rel) → its LIVE world position (attractor's current pos + rel).
local function rel_world(bodies, anchor, rx, ry, rz)
  local b = bodies[anchor]
  if not b then return nil end
  return b.x + rx, b.y + ry, b.z + rz
end

-- Draw a rest-frame polyline: each point rides its own anchor body's live pos.
local function draw_rel_polyline(rpts, anch, bodies, r, g, b, a)
  local n = math.floor(#rpts / 3)
  local px, py, pz, has = 0, 0, 0, false
  for k = 0, n - 1 do
    local wx, wy, wz = rel_world(bodies, anch[k + 1], rpts[k * 3 + 1], rpts[k * 3 + 2], rpts[k * 3 + 3])
    if wx then
      if has then draw.line(px, py, pz, wx, wy, wz, r, g, b, a) end
      px, py, pz, has = wx, wy, wz, true
    else
      has = false
    end
  end
end

-- The trajectory point nearest the cursor in SCREEN space (KSP click-on-line).
-- Projects each rest-frame point at its anchor's LIVE position (so you click the
-- orbit you SEE). Returns index k (0-based), the live world point, and its
-- time-from-now, or nil if nothing is within the pixel threshold / no camera.
local function pick_traj_point(traj, bodies)
  if not traj or not camera or not camera.exists() then return nil end
  local mx, my = input.mouse()
  local n = math.floor(#traj.rpts / 3)
  local bk, bd, bx, by, bz
  for k = 0, n - 1 do
    local px, py, pz = rel_world(bodies, traj.anch[k + 1],
      traj.rpts[k * 3 + 1], traj.rpts[k * 3 + 2], traj.rpts[k * 3 + 3])
    if px then
      local sx, sy, _, on = camera.worldToScreen(px, py, pz)
      if on then
        local d = (sx - mx) ^ 2 + (sy - my) ^ 2
        if not bd or d < bd then bd, bk, bx, by, bz = d, k, px, py, pz end
      end
    end
  end
  if bd and bd < (24 * 24) then
    return bk, bx, by, bz, (traj.t0 or 0) + bk * (traj.step or 0)
  end
  return nil
end

-- A little 3D diamond marker (used for the node + encounter points).
local function draw_diamond(x, y, z, s, r, g, b)
  draw.line(x - s, y, z, x, y + s, z, r, g, b, 1)
  draw.line(x, y + s, z, x + s, y, z, r, g, b, 1)
  draw.line(x + s, y, z, x, y - s, z, r, g, b, 1)
  draw.line(x, y - s, z, x - s, y, z, r, g, b, 1)
  draw.line(x, y, z - s, x, y + s, z, r, g, b, 1)
  draw.line(x, y + s, z, x, y, z + s, r, g, b, 1)
  draw.line(x, y, z + s, x, y - s, z, r, g, b, 1)
  draw.line(x, y - s, z, x, y, z - s, r, g, b, 1)
end

-- FROZEN COAST REFERENCE: while the ship coasts its orbit is analytically
-- fixed, so every trajectory walk + the node projection starts from this
-- frozen (state, time) — the walks sample the SAME absolute instants every
-- recompute, so the drawn path and the maneuver node are rock-steady. (They
-- used to re-derive from the live sim state every 0.15 s: sim noise + step
-- re-quantization made the node hop and the post-burn path flicker even in a
-- perfectly stable orbit.) Re-seeded when: thrusting or grounded, the
-- dominant body changed, the live state drifted off the reference conic
-- (RCS puffs, collisions, burns), or the ref aged past the walk window.
local traj_ref = nil -- { t, rx..rz, rvx..rvz (dominant-frame), dbname, span }

-- Recompute the maneuver-node projection + both trajectory walks (throttled).
-- `db` is the ship's dominant body; `o` its current conic (space.elements).
local function recompute_trajectories(node, db, bodies, pidx, o)
  -- Find the dominant body's index for the walkers.
  local dbi
  for i, b in ipairs(bodies) do
    if b.name == db.name then dbi = i break end
  end
  if not dbi then traj_now, traj_mnv = nil, nil return end

  local now = space.time()
  local rx0, ry0, rz0 = node.x - db.x, node.y - db.y, node.z - db.z
  local coasting = throttle < 0.005 and not node.grounded
  local stale = not coasting or not traj_ref or traj_ref.dbname ~= db.name
  if not stale then
    local dtb = now - traj_ref.t
    if dtb > traj_ref.span * 0.35 then
      stale = true -- the walk window has drifted behind us: re-anchor
    else
      -- Drift gate: where the ref conic says we are now vs the live sim.
      -- Catches anything that actually changed the orbit (RCS, a bump).
      local prx, pry, prz = space.propagate(traj_ref.rx, traj_ref.ry, traj_ref.rz,
        traj_ref.rvx, traj_ref.rvy, traj_ref.rvz, db.mu, dtb)
      local ex, ey, ez = prx - rx0, pry - ry0, prz - rz0
      local tol = math.max(4.0, (rx0 * rx0 + ry0 * ry0 + rz0 * rz0) * 4e-6)
      if ex * ex + ey * ey + ez * ez > tol then stale = true end
    end
  end
  if stale then
    -- Timescale: your own year if bound, else the planet's year (transfer clock).
    local span
    if o and o.period then
      span = o.period * 1.6
    else
      local yp = body_period(bodies, pidx, dbi)
      span = (yp and yp * 1.2) or 20000.0
    end
    traj_ref = { t = now, rx = rx0, ry = ry0, rz = rz0,
      rvx = node.vx, rvy = node.vy, rvz = node.vz, dbname = db.name, span = span }
  end
  local span = traj_ref.span
  local segs = 140
  local toff = traj_ref.t - now -- ≤ 0: walks start at the frozen ref instant

  -- Ship WORLD state AT THE REF INSTANT: the dominant body's state then + the
  -- frozen relative state (node.vx/vy/vz are frame-relative by convention).
  local dsx, dsy, dsz, dsvx, dsvy, dsvz = body_state_at(bodies, pidx, dbi, toff)
  local swx, swy, swz = dsx + traj_ref.rx, dsy + traj_ref.ry, dsz + traj_ref.rz
  local swvx, swvy, swvz = dsvx + traj_ref.rvx, dsvy + traj_ref.rvy, dsvz + traj_ref.rvz

  local rpts, anch, enc = walk_trajectory(bodies, pidx, swx, swy, swz, swvx, swvy, swvz, dbi, toff, span, segs)
  -- Rest-frame cache: rpts are RELATIVE to per-point anchor bodies (anch), drawn
  -- at each anchor's LIVE position. `step`/`t0` give each point a TIME (offset
  -- from now), which is what click-to-place reads to drop the node at the
  -- right spot on the orbit.
  traj_now = { rpts = rpts, anch = anch, enc = enc, t0 = toff, step = span / segs }

  if mnv then
    -- Walk the CURRENT patched conic to the node time, CROSSING any SOI between
    -- now and then, so the node's state (position, velocity, and which body it
    -- orbits) is the true one — not a single two-body arc around the start.
    -- `mnv.t0` is an ABSOLUTE sim-time instant; the LEAD (seconds until the burn)
    -- is `t0 − now`, which counts down ON ITS OWN as you coast — so the node stays
    -- PINNED to its point on the orbit while you close on it. (It used to store a
    -- fixed lead-from-now, so the walk always advanced the same span from an
    -- ever-advancing start → the node marched forever ahead and you never reached it.)
    -- Walk from the FROZEN ref instant to the node's absolute time: both ends
    -- are constants while coasting, so the step count and every SOI-crossing
    -- decision quantize identically each recompute — the node state (and the
    -- whole post-burn path) is bit-stable instead of hopping every 0.15 s.
    local t_node = math.max(mnv.t0 - now, 0)
    local nsteps = math.max(8, math.ceil((t_node - toff) / (span / segs)))
    local ns = patched_state_at(bodies, pidx, swx, swy, swz,
      swvx, swvy, swvz, dbi, toff, t_node, nsteps)
    local ndb = ns.anchor -- the body the node orbits (may DIFFER from the start db!)
    local nrx, nry, nrz = ns.rx, ns.ry, ns.rz
    local nvx, nvy, nvz = ns.rvx, ns.rvy, ns.rvz
    -- Burn basis at the node, IN ITS dominant body's frame: prograde / normal /
    -- radial-out (world axes — celestial frames only translate).
    local px, py, pz = norm(nvx, nvy, nvz)
    local hnx, hny, hnz = norm(cross(nrx, nry, nrz, nvx, nvy, nvz))
    local rdx, rdy, rdz = cross(px, py, pz, hnx, hny, hnz)
    local bx = px * mnv.pro + hnx * mnv.nor + rdx * mnv.rad
    local by = py * mnv.pro + hny * mnv.nor + rdy * mnv.rad
    local bz = pz * mnv.pro + hnz * mnv.nor + rdz * mnv.rad
    -- Post-burn seed: node WORLD position unchanged, WORLD velocity += the burn.
    local mwvx, mwvy, mwvz = ns.wvx + bx, ns.wvy + by, ns.wvz + bz
    -- Post-burn timescale about the node's body (bound → its year; escape → the
    -- transfer clock).
    local bvx, bvy, bvz = nvx + bx, nvy + by, nvz + bz
    local v2 = bvx * bvx + bvy * bvy + bvz * bvz
    local rlen = math.sqrt(nrx * nrx + nry * nry + nrz * nrz)
    local a2 = 1.0 / (2.0 / rlen - v2 / ns.mu)
    local mspan
    if a2 > 0 then
      mspan = 2 * math.pi * math.sqrt(a2 * a2 * a2 / ns.mu) * 1.6
    else
      local yp = body_period(bodies, pidx, ndb)
      mspan = (yp and yp * 1.2) or 20000.0
    end
    local mrpts, manch, menc =
      walk_trajectory(bodies, pidx, ns.wx, ns.wy, ns.wz, mwvx, mwvy, mwvz, ndb, t_node, mspan, segs)
    local dv = math.sqrt(mnv.pro ^ 2 + mnv.nor ^ 2 + mnv.rad ^ 2)
    -- Cache the burn's WORLD direction so the SAS "node" hold can point at it in
    -- flight (where recompute doesn't run).
    if dv > 1e-4 then mnv.bwx, mnv.bwy, mnv.bwz = norm(bx, by, bz) else mnv.bwx = nil end
    traj_mnv = {
      rpts = mrpts, anch = manch, enc = menc,
      m_anchor = ndb, m_rx = nrx, m_ry = nry, m_rz = nrz, -- node marker (its body's frame)
      bx = bx, by = by, bz = bz,
      dv = dv, t0 = t_node, step = mspan / segs,
    }
  else
    traj_mnv = nil
  end
end

local function update_map3d(node, dt)
  if not map_view then return end
  if not cam_node then cam_node = find("Camera 1") end
  local bodies = space.bodies()
  local nf = #bodies + 1 -- focus slots: the SHIP first, then every body
  if input.pressed("tab") then
    map_focus = map_focus % nf + 1
    map_zoom = nil
    map_offx, map_offy, map_offz = 0.0, 0.0, 0.0
  end
  if map_focus > nf then map_focus = 1 end
  if input.pressed("1") then map_show.orbits = not map_show.orbits end
  if input.pressed("2") then map_show.soi = not map_show.soi end
  if input.pressed("3") then map_show.markers = not map_show.markers end

  -- Focusing a body (TAB) streams ITS real terrain in (terrain.warm — loads it
  -- if cold, keeps it resident while focused, however far the ship is). Every
  -- other far world stays a cheap impostor sphere; the planet under the SHIP is
  -- kept loaded by the ship's own physical presence, never by this camera.
  if map_focus > 1 and bodies[map_focus - 1] then
    terrain.warm(bodies[map_focus - 1].name)
  end
  local focus, fname, fradius
  if map_focus == 1 then
    if piloting or not astronaut then
      focus = { x = node.x, y = node.y, z = node.z }
      fname = "SHIP"
    else
      focus = { x = astronaut.x, y = astronaut.y, z = astronaut.z }
      fname = "YOU"
    end
    fradius = 6
  else
    focus = bodies[map_focus - 1]
    fname, fradius = focus.name, focus.radius
  end
  if not map_zoom then
    -- Auto-fit: opening on the ship/astronaut frames the WHOLE current orbit
    -- (apoapsis + body), not a close-up of the hull.
    if map_focus == 1 then
      local dn0 = space.dominant(focus.x, focus.y, focus.z)
      local b0 = dn0 and space.body(dn0)
      if b0 then
        local rr = math.sqrt((focus.x - b0.x) ^ 2 + (focus.y - b0.y) ^ 2 + (focus.z - b0.z) ^ 2)
        map_zoom = math.max(rr * 2.2, b0.radius * 3.5)
        local o = space.elements(node.x, node.y, node.z, node.vx, node.vy, node.vz)
        if o and o.apoapsis then map_zoom = math.max(map_zoom, o.apoapsis * 1.5) end
      else
        map_zoom = 400
      end
    else
      map_zoom = math.max(fradius * 6, 80)
    end
  end

  -- Blender-style navigation: the mouse is free (and does nothing) unless
  -- RIGHT CLICK is held — drag then ORBITS the selection; CTRL+RIGHT-drag
  -- PANS the view offset in the camera plane instead. Scroll / ↑↓ zooms.
  local dragging = input.button(1)
  input.setMouseLocked(dragging)
  local cp, sp2 = math.cos(map_pitch2), math.sin(map_pitch2)
  local cy3, sy3 = math.cos(map_yaw2), math.sin(map_yaw2)
  if dragging then
    local mdx, mdy = input.mouse_delta()
    if input.key("ctrl") then
      -- Pan: move the offset along the camera's right/up axes, scaled by zoom.
      local rx3, rz3 = cy3, -sy3 -- camera right (horizontal)
      local uxx = -sp2 * sy3
      local uyy = cp
      local uzz = -sp2 * cy3 -- camera up
      local k = map_zoom * 0.0016
      map_offx = map_offx + (-mdx * rx3 + mdy * uxx) * k
      map_offy = map_offy + (mdy * uyy) * k
      map_offz = map_offz + (-mdx * rz3 + mdy * uzz) * k
    else
      map_yaw2 = map_yaw2 - mdx * 0.0035
      map_pitch2 = math.max(-1.45, math.min(1.45, map_pitch2 - mdy * 0.0035))
      cp, sp2 = math.cos(map_pitch2), math.sin(map_pitch2)
      cy3, sy3 = math.cos(map_yaw2), math.sin(map_yaw2)
    end
  end
  local sc = input.scroll()
  if sc and sc ~= 0 then map_zoom = map_zoom * (1 - sc * 0.1) end
  if input.key("up") then map_zoom = map_zoom * (1 - 1.5 * dt) end
  if input.key("down") then map_zoom = map_zoom * (1 + 1.5 * dt) end
  map_zoom = math.max(fradius * 1.6, math.min(map_zoom, 200000))
  if cam_node then
    local dx3, dy3, dz3 = cp * sy3, sp2, cp * cy3
    local lx = focus.x + map_offx
    local ly = focus.y + map_offy
    local lz = focus.z + map_offz
    cam_node.x = lx + dx3 * map_zoom
    cam_node.y = ly + dy3 * map_zoom
    cam_node.z = lz + dz3 * map_zoom
    cam_node.yaw = math.atan2(dx3, dz3)
    cam_node.pitch = -map_pitch2
    cam_node.roll = 0
  end

  -- Orbit lines + SOI rings for every body around its inferred parent.
  for i, b in ipairs(bodies) do
    local par = body_parent(bodies, i)
    if par then
      local has, p, ecc, e1x, e1y, e1z, e2x, e2y, e2z = orbit_basis(
        b.x - par.x, b.y - par.y, b.z - par.z,
        b.vx - par.vx, b.vy - par.vy, b.vz - par.vz, par.mu)
      if has then
        if map_show.orbits then
          draw_conic(par.x, par.y, par.z, e1x, e1y, e1z, e2x, e2y, e2z,
            p, ecc, 0.55, 0.62, 0.72, 0.8)
        end
        if map_show.soi and b.soi > 0 then
          draw_ring(b.x, b.y, b.z, e1x, e1y, e1z, e2x, e2y, e2z,
            b.soi, 0.35, 0.5, 0.62, 0.5)
        end
      end
    end
  end

  -- The ship's own conic + Pe/Ap markers + a position cross.
  local dn = space.dominant(node.x, node.y, node.z)
  local db = dn and space.body(dn)
  local info_orbit = nil
  if db then
    -- node velocity is ALREADY in the dominant body's carried frame (the
    -- engine's SOI frame convention) — subtracting db's world velocity again
    -- bent the drawn conic once the planet itself started orbiting the star.
    local has, p, ecc, e1x, e1y, e1z, e2x, e2y, e2z = orbit_basis(
      node.x - db.x, node.y - db.y, node.z - db.z,
      node.vx, node.vy, node.vz, db.mu)
    if has then
      info_orbit = { body = db.name, p = p, ecc = ecc, radius = db.radius }
      if map_show.orbits then
        draw_conic(db.x, db.y, db.z, e1x, e1y, e1z, e2x, e2y, e2z,
          p, ecc, 0.35, 0.85, 1.0, 1.0)
      end
      if map_show.markers then
        local ms = map_zoom * 0.012
        local rpe = p / (1 + ecc)
        draw_cross(db.x + e1x * rpe, db.y + e1y * rpe, db.z + e1z * rpe, ms, 1.0, 0.8, 0.25)
        if ecc < 1 then
          local rap = p / (1 - ecc)
          draw_cross(db.x - e1x * rap, db.y - e1y * rap, db.z - e1z * rap, ms, 0.4, 1.0, 0.6)
        end
      end
    end
  end
  if map_show.markers then
    draw_cross(node.x, node.y, node.z, map_zoom * 0.008, 0.6, 1.0, 0.7)
  end

  -- ---- maneuver-node planning (KSP) --------------------------------------
  -- Plan a burn and watch the resulting orbit — and where it drops you into
  -- another body's sphere of influence. Only while piloting a craft that has a
  -- real trajectory. The flight stick is frozen in the map (see fixedUpdate),
  -- so WASD/QE re-purpose to tune the burn. LEFT-CLICK the orbit line to place
  -- the node (drag to move it); ←/→ fine-tune its time; N clears; X zeroes ΔV.
  local oe = db and space.elements(node.x, node.y, node.z, node.vx, node.vy, node.vz)
  local hover_x, hover_y, hover_z -- the orbit point under the cursor (draw below)
  if piloting and db and not node.grounded then
    local pidx = parent_indices(bodies)
    local vref = math.sqrt(node.vx ^ 2 + node.vy ^ 2 + node.vz ^ 2)
    local dvrate = math.max(0.5, vref * 0.12)

    -- CLICK-ON-ORBIT placement (KSP): hover the current-orbit line, LEFT-click or
    -- drag to create/move the burn at that exact point (RMB is the camera, so LMB
    -- is free). Picks against the cached forward path, whose points carry a time —
    -- so the click lands the node at the right spot on the orbit.
    local pk_k, px, py, pz, pk_t = pick_traj_point(traj_now, bodies)
    if pk_k then hover_x, hover_y, hover_z = px, py, pz end
    if pk_t and input.button(0) then
      -- Store the node as an ABSOLUTE instant (now + the clicked point's lead) so it
      -- stays fixed on the orbit as you coast toward it instead of running ahead.
      local t0 = space.time() + pk_t
      if mnv then mnv.t0, mnv.burned = t0, 0.0
      else mnv = { t0 = t0, pro = 0.0, nor = 0.0, rad = 0.0, burned = 0.0 } end
    end

    -- N still works as a keyboard fallback (create at a lead / clear).
    if input.pressed("n") then
      if mnv then
        mnv = nil
      else
        local lead = (oe and oe.period and oe.period * 0.25) or 60.0
        mnv = { t0 = space.time() + lead, pro = 0.0, nor = 0.0, rad = 0.0, burned = 0.0 }
      end
    end
    if mnv then
      -- ←/→ fine-tune the node time; W/S/A/D/Q/E tune the ΔV; X zeroes it.
      local tref = (oe and oe.period) or 600.0
      local horizon = (oe and oe.period and oe.period * 1.5) or (tref * 6)
      local sliding = input.key("left") or input.key("right")
      if input.key("left") then mnv.t0 = mnv.t0 - tref * 0.15 * dt end
      if input.key("right") then mnv.t0 = mnv.t0 + tref * 0.15 * dt end
      -- Clamp ONLY while actively sliding (keep the handle ahead of you and inside
      -- the horizon) — never during the passive countdown, so the lead can fall to
      -- 0 and past as you coast up to the node.
      if sliding then
        local now = space.time()
        mnv.t0 = math.max(now + 1.0, math.min(mnv.t0, now + horizon))
      end
      if input.key("w") then mnv.pro = mnv.pro + dvrate * dt end
      if input.key("s") then mnv.pro = mnv.pro - dvrate * dt end
      if input.key("d") then mnv.rad = mnv.rad + dvrate * dt end
      if input.key("a") then mnv.rad = mnv.rad - dvrate * dt end
      if input.key("q") then mnv.nor = mnv.nor + dvrate * dt end
      if input.key("e") then mnv.nor = mnv.nor - dvrate * dt end
      if input.pressed("x") then mnv.pro, mnv.nor, mnv.rad = 0, 0, 0 end
    end
    if time - traj_t >= 0.15 then
      traj_t = time
      recompute_trajectories(node, db, bodies, pidx, oe)
    end
  else
    mnv = nil
    traj_now, traj_mnv = nil, nil
  end

  -- Draw the cached walks in the dominant body's REST FRAME: every point rides
  -- its anchor body's LIVE position, so the orbit is a static ellipse around the
  -- planet where you SEE it, not smeared along the planet's own orbit.
  local function enc_diamond(e, s, r, g, b)
    local ex, ey, ez = rel_world(bodies, e.anchor, e.rx, e.ry, e.rz)
    if ex then draw_diamond(ex, ey, ez, s, r, g, b) end
  end
  if traj_now and map_show.markers then
    for _, e in ipairs(traj_now.enc) do
      if e.kind == "impact" then
        enc_diamond(e, map_zoom * 0.014, 1.0, 0.4, 0.3)
      else
        enc_diamond(e, map_zoom * 0.014, 0.4, 0.9, 1.0)
      end
    end
  end
  -- The orbit point under the cursor (a soft ring) — where a click drops the node.
  if hover_x and not mnv then
    draw_diamond(hover_x, hover_y, hover_z, map_zoom * 0.012, 0.7, 0.95, 1.0)
  end
  if traj_mnv then
    if map_show.orbits then
      draw_rel_polyline(traj_mnv.rpts, traj_mnv.anch, bodies, 1.0, 0.55, 0.15, 0.95) -- post-burn (amber)
    end
    local mx, my, mz = rel_world(bodies, traj_mnv.m_anchor, traj_mnv.m_rx, traj_mnv.m_ry, traj_mnv.m_rz)
    if mx then
      draw_diamond(mx, my, mz, map_zoom * 0.016, 1.0, 0.7, 0.2)
      if traj_mnv.dv > 1e-4 then
        local bnx, bny, bnz = norm(traj_mnv.bx, traj_mnv.by, traj_mnv.bz)
        local bl = map_zoom * 0.06
        draw.line(mx, my, mz, mx + bnx * bl, my + bny * bl, mz + bnz * bl, 1.0, 0.85, 0.3, 1.0)
      end
    end
    if map_show.markers then
      for _, e in ipairs(traj_mnv.enc) do
        if e.kind == "enter" then
          enc_diamond(e, map_zoom * 0.018, 0.4, 1.0, 0.5)
        elseif e.kind == "impact" then
          enc_diamond(e, map_zoom * 0.018, 1.0, 0.4, 0.3)
        else
          enc_diamond(e, map_zoom * 0.018, 0.9, 0.9, 0.5)
        end
      end
    end
  end

  -- Focused-body info panel (the flight HUD stands down while the map is open).
  if time - map_hud_t >= 0.1 then
    map_hud_t = time
    local lines = {}
    lines[1] = string.format("MAP  ·  focus %s   (TAB cycle · RMB-drag orbit · CTRL+RMB pan · scroll/↑↓ zoom)", fname)
    if map_focus > 1 then
      local b = bodies[map_focus - 1]
      local dx4, dy4, dz4 = node.x - b.x, node.y - b.y, node.z - b.z
      local dd = math.sqrt(dx4 * dx4 + dy4 * dy4 + dz4 * dz4)
      lines[2] = string.format("radius %.0f   µ %.3g   SOI %s", b.radius, b.mu,
        b.soi < 0 and "∞" or string.format("%.0f", b.soi))
      lines[3] = string.format("ship distance %.0f  (alt over it %+.0f)", dd, dd - b.radius)
    end
    if info_orbit then
      local o = info_orbit
      if o.ecc < 1 then
        lines[#lines + 1] = string.format("SHIP ORBIT [%s]  pe %+.0f  ap %+.0f  e %.2f",
          o.body, o.p / (1 + o.ecc) - o.radius, o.p / (1 - o.ecc) - o.radius, o.ecc)
      else
        lines[#lines + 1] = string.format("SHIP ESCAPE [%s]  pe %+.0f  e %.2f",
          o.body, o.p / (1 + o.ecc) - o.radius, o.ecc)
      end
    end
    -- Maneuver node + encounter readout.
    if mnv then
      local dv = math.sqrt(mnv.pro ^ 2 + mnv.nor ^ 2 + mnv.rad ^ 2)
      local lead = mnv.t0 - space.time()
      lines[#lines + 1] = string.format(
        "NODE  T%s%.0fs  ΔV %.1f   pro %+.1f · rad %+.1f · nor %+.1f",
        lead >= 0 and "-" or "+", math.abs(lead), dv, mnv.pro, mnv.rad, mnv.nor)
      if traj_mnv and #traj_mnv.enc > 0 then
        local e = traj_mnv.enc[1]
        local verb = e.kind == "enter" and "ENCOUNTER" or (e.kind == "impact" and "IMPACT" or "exit")
        lines[#lines + 1] = string.format("  ⇒ %s %s in %.0fs", verb, e.name, e.t)
      end
      lines[#lines + 1] = "  LMB-drag on orbit moves it · W/S prograde · A/D radial · Q/E normal · X zero · N clear"
    elseif piloting and not node.grounded then
      if traj_now and #traj_now.enc > 0 then
        local e = traj_now.enc[1]
        local verb = e.kind == "enter" and "ENCOUNTER" or (e.kind == "impact" and "IMPACT" or "SOI exit")
        lines[#lines + 1] = string.format("%s: %s in %.0fs   ·   click the orbit to plan a burn", verb, e.name, e.t)
      else
        lines[#lines + 1] = "Left-click your orbit line to plan a maneuver (or N)"
      end
    end
    lines[#lines + 1] = string.format(
      "1 orbits %s · 2 SOIs %s · 3 markers %s · M close",
      map_show.orbits and "ON" or "off",
      map_show.soi and "ON" or "off",
      map_show.markers and "ON" or "off")
    set_hud(node, table.concat(lines, "\n"))
  end
end

-- The world-space direction the SAS autopilot should point the nose at, for the
-- current mode — or nil if it's undefined (prograde/normal/radial need real
-- motion; node needs a planned burn). Velocities are the dominant-body-frame
-- values node.vx/vy/vz (= velocity RELATIVE to the attractor), which is exactly
-- what prograde/normal/radial want; do NOT add the body's world velocity.
local function sas_target_dir(node, db, mode)
  if not db then return nil end
  local vx, vy, vz = node.vx, node.vy, node.vz
  local vl = math.sqrt(vx * vx + vy * vy + vz * vz)
  if mode == "node" then
    -- The burn's WORLD direction, computed across SOI transitions and cached by
    -- recompute_trajectories when you place/edit the node (recompute doesn't run
    -- in flight; the burn direction at a fixed orbital point barely drifts).
    if mnv and mnv.bwx then return mnv.bwx, mnv.bwy, mnv.bwz end
    return nil
  end
  if vl < 2.0 then return nil end -- prograde/normal/radial are undefined at rest
  local pgx, pgy, pgz = vx / vl, vy / vl, vz / vl
  if mode == "prograde" then return pgx, pgy, pgz end
  if mode == "retrograde" then return -pgx, -pgy, -pgz end
  local hx, hy, hz = cross(node.x - db.x, node.y - db.y, node.z - db.z, vx, vy, vz)
  local nmx, nmy, nmz = norm(hx, hy, hz)
  if nmx == 0 and nmy == 0 and nmz == 0 then return nil end
  if mode == "normal" then return nmx, nmy, nmz end
  if mode == "antinormal" then return -nmx, -nmy, -nmz end
  local rox, roy, roz = cross(pgx, pgy, pgz, nmx, nmy, nmz) -- radial out
  if mode == "radialout" then return rox, roy, roz end
  if mode == "radialin" then return -rox, -roy, -roz end
  return nil
end

-- Map number keys → SAS modes (flight only; 1/2/3 are map toggles in map view).
local SAS_KEYS = {
  ["1"] = "stability", ["2"] = "prograde", ["3"] = "retrograde",
  ["4"] = "normal", ["5"] = "antinormal", ["6"] = "radialin",
  ["7"] = "radialout", ["8"] = "node",
}
-- Short labels for the HUD.
local SAS_LABEL = {
  off = "OFF", stability = "STAB", prograde = "PRO", retrograde = "RETRO",
  normal = "NML", antinormal = "ANTI-NML", radialin = "RAD-IN",
  radialout = "RAD-OUT", node = "NODE",
}

-- Set the SAS mode. PUBLISHED (global) so the HUD buttons (sas_button.lua) call
-- it: `findScript("ship_controller").setSAS("prograde")`. Remembers the last
-- non-off mode so the T key can re-enable it.
function setSAS(m)
  sas_mode = m
  if m ~= "off" then sas_last = m end
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
      map_view = false
      mnv = nil -- drop any planned burn when you leave the seat
    elseif distance(astronaut, node) <= params.board_range then
      piloting = true
      astronaut.visible = false
      set_navball(true)
      -- Board in a STABILITY hold — never inherit a velocity-pointing mode into a
      -- takeoff. On a vertical climb your velocity (in the planet's frame) points
      -- radially OUT, so an inherited "retrograde" would aim the nose straight at
      -- the core and fight you off the pad. You pick prograde/retrograde/etc. once
      -- you're actually orbiting.
      sas_mode, sas_last = "stability", "stability"
      spawn_t = math.max(spawn_t, time - params.grace + 0.75) -- brief settle grace
    end
  end
  -- Parked astronaut rides inside the hull (bodies don't push each other).
  if piloting and astronaut then
    astronaut.x, astronaut.y, astronaut.z = node.x, node.y, node.z
    astronaut.vx, astronaut.vy, astronaut.vz = node.vx, node.vy, node.vz
  end

  -- ---- the map works on foot too (Ty: open it from the astronaut) ---------
  if input.pressed("m") then
    map_view = not map_view
    map_zoom = nil -- re-fit to the focus every time it opens
    map_focus = 1 -- always open on yourself
    map_offx, map_offy, map_offz = 0.0, 0.0, 0.0
    -- The instrument cluster stands down while the map owns the screen.
    set_navball(not map_view and piloting)
    if not map_view then set_hud(node, piloting and "" or nil) end
  end
  -- The map itself (camera + line drawing) runs in lateUpdate — the CAMERA
  -- pass. From fixedUpdate it sampled tick poses while the world renders
  -- interpolated ones, which showed as constant back-and-forth jitter.

  if not piloting then
    -- ---- summon the ship: L places it right in front of you (testing / a
    -- lost ship). Un-wrecks, refuels, lands upright on its gear.
    if input.pressed("l") and astronaut then
      local dxs, dys, dzs = 0, 0, 1
      if not cam_node then cam_node = find("Camera 1") end
      if cam_node then
        dxs, dys, dzs = norm(astronaut.x - cam_node.x, astronaut.y - cam_node.y,
          astronaut.z - cam_node.z)
      end
      local d0 = space.dominant(astronaut.x, astronaut.y, astronaut.z)
      local b0 = d0 and space.body(d0)
      local ux, uy, uz = 0, 1, 0
      if b0 then
        ux, uy, uz = norm(astronaut.x - b0.x, astronaut.y - b0.y, astronaut.z - b0.z)
      end
      node.x = astronaut.x + dxs * 10 + ux * 4
      node.y = astronaut.y + dys * 10 + uy * 4
      node.z = astronaut.z + dzs * 10 + uz * 4
      wrecked = false
      reset_pose(node)
      -- Ride the player's frame (matters when summoning in orbit/space).
      node.vx, node.vy, node.vz = astronaut.vx, astronaut.vy, astronaut.vz
      set_ship_visible(node, true)
      legs_deployed, leg_anim = true, 1.0
      animate_legs(0)
      for _, d in ipairs(debris) do destroy(d) end
      debris = {}
      print("ship summoned")
    end
    if not map_view then set_hud(node, nil) end
    pvx, pvy, pvz = node.vx, node.vy, node.vz
    return
  end

  -- ---- wreck & respawn ----------------------------------------------------
  if wrecked then
    mnv = nil -- a wrecked ship has no trajectory to plan
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

  -- SAS: T toggles off / on (restoring the last hold mode); number keys pick a
  -- hold mode in flight (guarded — 1/2/3 are the map's indicator toggles).
  if input.pressed("t") then
    if sas_mode == "off" then
      sas_mode = sas_last
    else
      sas_last = sas_mode
      sas_mode = "off"
    end
  end
  if not map_view then
    for k, m in pairs(SAS_KEYS) do
      if input.pressed(k) then
        sas_mode = m
        sas_last = m
      end
    end
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
  local map_drag2 = map_view and input.button(1)
  local control_touched = ((input.key("shift") or input.key("ctrl")) and not map_drag2)
    or input.key("z")
    or input.key("w") or input.key("a") or input.key("s") or input.key("d")
    or input.key("q") or input.key("e")
  if warp > 1.001 then
    if control_touched then
      space.warp(1)
      warp_note, warp_note_t = "warp canceled — pilot input", time
    end
    -- Coasting on rails: HUD only; attitude/thrust/brake wait for realtime.
    set_flame(node, false, 0)
    if not map_view and time - hud_t >= 0.1 then
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
    pvx, pvy, pvz = node.vx, node.vy, node.vz
    return
  end

  -- ---- throttle + fuel -----------------------------------------------------
  -- CTRL/SHIFT during a map RIGHT-drag are camera gestures, not throttle.
  local map_drag = map_view and input.button(1)
  if input.key("shift") and not map_drag then throttle = throttle + params.throttle_rate * dt end
  if input.key("ctrl") and not map_drag then throttle = throttle - params.throttle_rate * dt end
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
  -- the nose UP, push forward (W) pitches it DOWN. While the MAP is open the
  -- stick is frozen — those same keys tune the maneuver node instead (you're
  -- planning, not flying), which also kills the old "WASD rotates the ship in
  -- the background of the map" quirk.
  local p, y, r = 0, 0, 0
  if not map_view then
    p = (input.key("w") and 1 or 0) - (input.key("s") and 1 or 0)
    y = (input.key("a") and 1 or 0) - (input.key("d") and 1 or 0)
    r = (input.key("e") and 1 or 0) - (input.key("q") and 1 or 0)
  end
  local sasm_x, sasm_y, sasm_z -- the SAS aim point, for the navball marker
  -- Debounce the flickery per-contact grounded flag. A sphere hull resting on
  -- voxel terrain (and a ship settled a hair INTO a slope — the pad sits the ship
  -- slightly sunk) drops contact for a tick constantly; a generous latch keeps a
  -- landed ship in the TOPPLE + parking-brake model instead of flipping to
  -- free-flight SAS, which — in a velocity-pointing mode — would chase the noisy
  -- pushout velocity and swing the nose at the core. On the ground at low throttle
  -- the ship topples rather than free-spins; firing the engine (throttle ≥ 0.15)
  -- or truly leaving the surface hands back to free flight.
  if node.grounded then grounded_until = time + 0.35 end
  local landed = node.grounded or time < grounded_until
  local on_ground = throttle < 0.15 and landed
  if on_ground then
    avp, avy, avr = 0, 0, 0 -- no orbital rates leak onto the ground
    apply_topple(node, dt, p, y)
  else
    tip_w, toppled = 0.0, false -- airborne: drop any lean state
    local step = params.rate_accel * dt
    -- Axis wiring (the pilot's fix): for a rocket whose long axis is `nose`,
    -- PITCH tilts about `right`, YAW tilts the nose left/right about `fwd` (the
    -- belly axis), and ROLL spins about `nose`. The autopilot decomposes its
    -- desired world angular velocity onto this SAME basis, so it can't fight it.
    local rx2, ry2, rz2 = cross(fx, fy, fz, nx, ny, nz) -- ship right
    local manual = (p ~= 0 or y ~= 0 or r ~= 0)
    local sas_on = sas_mode ~= "off"
    local want_p, want_y, want_r
    if manual then
      -- Hands on the stick always win: touched axes command a rate, released
      -- axes damp to 0 (SAS on) or coast (off).
      local hold = sas_on and 0 or nil
      want_p = p ~= 0 and p * params.max_rate or (hold or avp)
      want_y = y ~= 0 and y * params.max_rate or (hold or avy)
      want_r = r ~= 0 and r * params.max_rate or (hold or avr)
    elseif sas_mode == "off" then
      want_p, want_y, want_r = avp, avy, avr -- pure Newton: rates persist
    elseif sas_mode == "stability" then
      want_p, want_y, want_r = 0, 0, 0 -- damp rotation to zero
    else
      -- Pointing autopilot: rotate the nose toward the mode's target direction.
      local dbf = space.body(space.dominant(node.x, node.y, node.z))
      local tx, ty, tz = sas_target_dir(node, dbf, sas_mode)
      if tx then
        sasm_x, sasm_y, sasm_z = tx, ty, tz -- show it on the navball
        local axx, axy, axz = cross(nx, ny, nz, tx, ty, tz)
        local s = math.sqrt(axx * axx + axy * axy + axz * axz)
        local c = nx * tx + ny * ty + nz * tz
        if s > 1e-5 then
          local theta = math.atan2(s, c)
          -- KSP braking law: the highest rate that can still stop within
          -- rate_accel before arrival → converges with no overshoot or hunting.
          local rate = math.min(params.max_rate, math.sqrt(2 * params.rate_accel * theta))
          local ox = (axx / s) * rate
          local oy = (axy / s) * rate
          local oz = (axz / s) * rate
          want_p = ox * rx2 + oy * ry2 + oz * rz2
          want_y = ox * fx + oy * fy + oz * fz
          want_r = 0 -- pointing modes don't command roll; let it damp
        else
          want_p, want_y, want_r = 0, 0, 0 -- already aligned
        end
      else
        want_p, want_y, want_r = 0, 0, 0 -- target undefined (at rest / no node)
      end
    end
    avp = toward(avp, want_p, step)
    avy = toward(avy, want_y, step)
    avr = toward(avr, want_r, step)

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
  end

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
  if landed and throttle < 0.01 and spd < params.park_speed then
    -- Parked: pin it still. Kills the low-speed grinding/sliding jitter a
    -- sphere hull otherwise does on voxel terrain (uses the debounced `landed`
    -- so a flickery contact frame can't briefly un-pin and let it drift/rotate).
    node.vx, node.vy, node.vz = 0, 0, 0
  else
    node.vx = node.vx + nx * acc * dt
    node.vy = node.vy + ny * acc * dt
    node.vz = node.vz + nz * acc * dt
  end
  set_flame(node, throttle > 0.02, throttle)

  -- ---- maneuver-node burn tracking ---------------------------------------
  -- Tally the ΔV actually applied ALONG the burn direction while you're in the
  -- burn window and pointing at the node, so the HUD can count "ΔV left" down to
  -- zero (KSP's shrinking node). Reset well before the window so every approach
  -- starts from the full planned ΔV. `mnv.bwx/y/z` is the node's (fixed, inertial)
  -- burn direction cached by the map; `acc`/`mass` are this tick's thrust state.
  if mnv and mnv.bwx then
    local lead = mnv.t0 - space.time()
    local dv = math.sqrt(mnv.pro ^ 2 + mnv.nor ^ 2 + mnv.rad ^ 2)
    local burn_dt = mass > 0 and dv / (params.max_thrust / mass) or 0
    if lead > burn_dt * 0.5 + 3.0 then
      mnv.burned = 0.0
    elseif throttle > 0.02 then
      local align = math.max(0, nx * mnv.bwx + ny * mnv.bwy + nz * mnv.bwz)
      mnv.burned = (mnv.burned or 0) + acc * align * dt
    end
  end

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

  update_navball(node, sasm_x, sasm_y, sasm_z)

  -- ---- HUD (10 Hz; the map's info panel owns the text while it's open) ----
  if not map_view and time - hud_t >= 0.1 then
    hud_t = time
    local dom = space.dominant(node.x, node.y, node.z)
    local b = dom and space.body(dom)
    local lines = {}
    local bars = math.floor(throttle * 10 + 0.5)
    lines[1] = string.format("THR [%s%s] %3d%%   SAS %s   GEAR %s%s",
      string.rep("|", bars), string.rep("·", 10 - bars), throttle * 100,
      SAS_LABEL[sas_mode] or "?",
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
    -- Maneuver-node readout in FLIGHT: time-to-node (counts down), planned ΔV, the
    -- estimated burn duration, and when to light the engine — start early so the
    -- burn straddles the node — then a live "BURN NOW — ΔV left" as you execute.
    if mnv then
      local lead = mnv.t0 - space.time()
      local dv = math.sqrt(mnv.pro ^ 2 + mnv.nor ^ 2 + mnv.rad ^ 2)
      local mass2 = params.mass - params.fuel_mass * (1.0 - fuel / params.fuel)
      local a_full = mass2 > 0 and params.max_thrust / mass2 or 0
      local burn_dt = a_full > 1e-4 and dv / a_full or 0
      local rem = math.max(0, dv - (mnv.burned or 0))
      local rem_dt = a_full > 1e-4 and rem / a_full or 0
      lines[#lines + 1] = string.format("NODE  T%s%.0fs   ΔV %.1f   burn ~%.1fs",
        lead >= 0 and "-" or "+", math.abs(lead), dv, burn_dt)
      if dv < 0.05 then
        lines[#lines + 1] = "  open map (M) to set ΔV · then SAS ⟶ NODE to aim"
      else
        local start_in = lead - burn_dt * 0.5
        if rem <= 0.05 then
          lines[#lines + 1] = "  ✔ burn complete — X zeroes ΔV · N clears the node"
        elseif start_in > 0.5 then
          lines[#lines + 1] = string.format("  hold SAS ⟶ NODE · start burn in %.0fs", start_in)
        elseif lead > -burn_dt then
          lines[#lines + 1] = string.format("  ▶▶ BURN NOW — ΔV %.1f left (~%.1fs)", rem, rem_dt)
        else
          lines[#lines + 1] = "  ⚠ node passed — N clears it (or replan on the map)"
        end
      end
    end
    if warp_note and time - warp_note_t < 2.5 then lines[#lines + 1] = "⚠ " .. warp_note end
    lines[#lines + 1] =
      "F exit·Shift/Ctrl thr·X cut·Z full·WASD/QE rotate·T SAS·click SAS buttons (left) for hold modes·B gear·./,warp·M map"
    set_hud(node, table.concat(lines, "\n"))
  end

  pvx, pvy, pvz = node.vx, node.vy, node.vz
end

-- The map runs in the CAMERA pass: after physics and the interpolated
-- writeback, so the map camera (and every drawn orbit line) samples the same
-- smooth poses the frame renders. Driving it from fixedUpdate stepped the
-- camera at tick rate against interpolated rendering = perpetual jitter.
function lateUpdate(node, dt)
  update_map3d(node, dt)
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
