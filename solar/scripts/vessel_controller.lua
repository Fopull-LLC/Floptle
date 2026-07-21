-- VESSEL CONTROLLER — flies a BUILT ship (the compound assembly rebuilt from
-- your blueprint) with the same cockpit feel as the scout ship: rate-commanded
-- attitude, SAS hold modes, the navball + tapes, a live HUD, real pooled fuel.
-- Thrust is honest: applied AT each engine's blueprint offset through the
-- compound physics — an off-CoM engine torques the stack, exactly as built.
--
--   F        board / exit the pod (walk to the hatch ring to re-board)
--   SPACE    clamped → release the launch clamps; flying → next stage
--   SHIFT / CTRL   throttle up / down    X  cut    Z  full
--   W/S      pitch (S pulls the nose UP)   A/D  yaw   Q/E  roll
--   T        SAS on/off    1-7  hold: 1 stability · 2 prograde · 3 retrograde
--            · 4 normal · 5 anti-nml · 6 radial-in · 7 radial-out
--   M        map (the scout ship's map focuses YOU — riding the pod counts)
--
-- Launching from the builder seats you in the pod from the first frame, with
-- the vessel CLAMPED (anchored) to the launchpad until you release. Fuel pools
-- across every tank on board; staging away tanks takes their share with them.

defaults = {
  torque = 60.0,        -- attitude authority per axis (vs the compound inertia)
  max_rate = 1.0,       -- commanded turn rate cap, rad/s
  rate_accel = 2.5,     -- pointing-mode braking law, rad/s²
  board_range = 3.4,    -- how close the astronaut must be to the pod to board
  throttle_rate = 0.5,  -- full throttle in 2 s of held SHIFT
  refuel_rate = 15.0,   -- units/s while clamped on the pad
}

-- Published (walker/camera defer & follow; SAS buttons read/drive these).
piloting = false
throttle = 0.0
fuel = 0.0
sas_mode = "stability"
local sas_last = "stability"

local bp = nil
local engines = {}     -- { {x,y,z, thrust, burn} } vessel-local, live only
local tanks = {}       -- { {y, fuel} } vessel-local, live only
local decouplers = {}  -- { {uid, y} } sorted bottom-up, fired in order
local pod = { x = 0, y = 0.5, z = 0 }
local part_total = 0
local fuel_cap = 0.0
local boarding = false
local astronaut, hud, hud_prompt
local hud_t = -10.0
local navball, tape_spd, tape_alt, txt_spd, txt_alt, txt_hdg
local warp_note = false

-- ── math helpers (same conventions as the scout ship) ───────────────────────
local function norm(x, y, z)
  local l = math.sqrt(x * x + y * y + z * z)
  if l < 1e-6 then return 0, 0, 0 end
  return x / l, y / l, z / l
end

local function cross(ax, ay, az, bx, by, bz)
  return ay * bz - az * by, az * bx - ax * bz, ax * by - ay * bx
end

-- Vessel-frame basis from the node's YXZ euler (matches the engine writeback).
-- The NOSE is the stack axis (+Y — the pod points up), fwd completes the frame.
local function basis(node)
  local cy, sy = math.cos(node.yaw), math.sin(node.yaw)
  local cx, sx = math.cos(node.pitch), math.sin(node.pitch)
  local cz, sz = math.cos(node.roll), math.sin(node.roll)
  local rx = vec3(cy * cz + sy * sx * sz, cx * sz, -sy * cz + cy * sx * sz)
  local up = vec3(-cy * sz + sy * sx * cz, cx * cz, sy * sz + cy * sx * cz)
  local rz = vec3(sy * cx, -sx, cy * cx)
  return rx, up, rz
end

-- ── blueprint ───────────────────────────────────────────────────────────────
local function load_bp()
  bp = save.get("shipyard.blueprint")
  engines, tanks, decouplers, part_total, fuel_cap = {}, {}, {}, 0, 0.0
  if not bp or not bp.parts then return end
  for _, d in pairs(bp.parts) do
    part_total = part_total + 1
    if (d.thrust or 0) > 0 then
      engines[#engines + 1] = { x = d.x, y = d.y, z = d.z, thrust = d.thrust, burn = d.burn or 1 }
    end
    if (d.fuel or 0) > 0 then
      tanks[#tanks + 1] = { y = d.y, fuel = d.fuel }
      fuel_cap = fuel_cap + d.fuel
    end
    if (d.decouple or 0) == 1 then
      decouplers[#decouplers + 1] = { uid = d.uid, y = d.y }
    end
    if d.kind == "crewed" then pod = { x = d.x, y = d.y, z = d.z } end
  end
  table.sort(decouplers, function(a, b) return a.y < b.y end)
  fuel = fuel_cap
end

local function pod_world(node)
  local rx, up, rz = basis(node)
  return node.x + rx.x * pod.x + up.x * pod.y + rz.x * pod.z,
         node.y + rx.y * pod.x + up.y * pod.y + rz.y * pod.z,
         node.z + rx.z * pod.x + up.z * pod.y + rz.z * pod.z
end

-- ── instruments (the scout ship's cluster, reused verbatim) ─────────────────
local function set_hud(text)
  if not hud then hud = find("Ship HUD Text") end
  if not hud then return end
  local el = hud:getcomponent("UiElement")
  if el then el.visible = text ~= nil end
  if text then hud.text = text end
end

local prompt_last = nil
local function set_prompt(text)
  if not hud_prompt then hud_prompt = find("Vessel HUD") end
  if hud_prompt and text ~= prompt_last then
    hud_prompt.text = text
    prompt_last = text
  end
end

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

-- Navball uniforms: the vessel basis in the LOCAL HORIZON frame (x=east,
-- y=radial up, z=north) + prograde + the SAS aim ring. Same math as the ship.
local function update_navball(node, nx, ny, nz, fx, fy, fz, tgtx, tgty, tgtz)
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
  if tgtx then
    navball:setShaderParam("sasTarget", toH(tgtx, tgty, tgtz))
  else
    navball:setShaderParam("sasTarget", 0, 0, 0)
  end
  local he = nx * ex + ny * ey + nz * ez
  local hn = nx * nhx + ny * nhy + nz * nhz
  local heading = (math.deg(math.atan2(he, hn)) + 360) % 360
  local dxr, dyr, dzr = node.x - b.x, node.y - b.y, node.z - b.z
  local alt = math.sqrt(dxr * dxr + dyr * dyr + dzr * dzr) - b.radius
  if tape_spd then tape_spd:setShaderParam("tape", vl, 40, 5) end
  if tape_alt then tape_alt:setShaderParam("tape", alt, 150, 25) end
  if txt_spd then txt_spd.text = string.format("%.0f", vl) end
  if txt_alt then txt_alt.text = string.format("%.0f", alt) end
  if txt_hdg then txt_hdg.text = string.format("HDG %03.0f°", heading) end
end

-- ── SAS ─────────────────────────────────────────────────────────────────────
local SAS_KEYS = {
  ["1"] = "stability", ["2"] = "prograde", ["3"] = "retrograde",
  ["4"] = "normal", ["5"] = "antinormal", ["6"] = "radialin",
  ["7"] = "radialout",
}
local SAS_LABEL = {
  off = "OFF", stability = "STAB", prograde = "PRO", retrograde = "RETRO",
  normal = "NML", antinormal = "ANTI-NML", radialin = "RAD-IN",
  radialout = "RAD-OUT",
}

function setSAS(m)
  sas_mode = m
  if m ~= "off" then sas_last = m end
end

local function sas_target_dir(node, db, mode)
  if not db then return nil end
  local vx, vy, vz = node.vx, node.vy, node.vz
  local vl = math.sqrt(vx * vx + vy * vy + vz * vz)
  if vl < 2.0 then return nil end
  local pgx, pgy, pgz = vx / vl, vy / vl, vz / vl
  if mode == "prograde" then return pgx, pgy, pgz end
  if mode == "retrograde" then return -pgx, -pgy, -pgz end
  local hx, hy, hz = cross(node.x - db.x, node.y - db.y, node.z - db.z, vx, vy, vz)
  local nmx, nmy, nmz = norm(hx, hy, hz)
  if nmx == 0 and nmy == 0 and nmz == 0 then return nil end
  if mode == "normal" then return nmx, nmy, nmz end
  if mode == "antinormal" then return -nmx, -nmy, -nmz end
  local rox, roy, roz = cross(pgx, pgy, pgz, nmx, nmy, nmz)
  if mode == "radialout" then return rox, roy, roz end
  if mode == "radialin" then return -rox, -roy, -roz end
  return nil
end

-- ── staging ─────────────────────────────────────────────────────────────────
-- Detaching everything at/below a decoupler also gives away its tanks' share
-- of the pooled fuel and forgets its engines.
local function drop_below(cut_y)
  local frac = (fuel_cap > 0) and (fuel / fuel_cap) or 0
  local keep_e, keep_t, cap = {}, {}, 0.0
  for _, e in ipairs(engines) do
    if e.y > cut_y + 0.01 then keep_e[#keep_e + 1] = e end
  end
  for _, t in ipairs(tanks) do
    if t.y > cut_y + 0.01 then
      keep_t[#keep_t + 1] = t
      cap = cap + t.fuel
    end
  end
  engines, tanks, fuel_cap = keep_e, keep_t, cap
  fuel = cap * frac
end

-- ── lifecycle ───────────────────────────────────────────────────────────────
function start(node)
  load_bp()
  if (save.get("shipyard.pilot") or 0) == 1 then
    save.set("shipyard.pilot", 0)
    boarding = true
  end
end

local runaway_logged = false
-- Clamp-release readiness: the full blueprint count, OR a part count that has
-- stopped changing (a blueprint part that never spawned must not weld the
-- clamps shut forever — the spawner logs what went missing).
local parts_seen, parts_t = -1, 0.0

local function assembly_ready(info)
  if #info.parts >= part_total then return true end
  if #info.parts ~= parts_seen then
    parts_seen, parts_t = #info.parts, time
  end
  return #info.parts > 0 and time - parts_t > 2.0
end

function fixedUpdate(node, dt)
  if not astronaut or not astronaut.valid then astronaut = find("Astronaut") end
  local info = assembly.info(node)

  -- Launch handoff: climb into the pod the moment the astronaut node exists.
  if boarding and astronaut then
    boarding = false
    piloting = true
    astronaut.visible = false
    set_navball(true)
    sas_mode, sas_last = "stability", "stability"
    log("vessel: you're in the pod — SHIFT throttle, SPACE releases the clamps")
  end

  -- Runaway diagnostic (an uncontrolled vessel past 50 u/s = report data).
  if info and not piloting and not runaway_logged then
    local v = info.vel
    local speed = math.sqrt(v.x * v.x + v.y * v.y + v.z * v.z)
    if speed > 50 then
      runaway_logged = true
      log(string.format(
        "VESSEL RUNAWAY: speed %.0f  pos (%.1f, %.1f, %.1f)  grounded=%s",
        speed, node.x, node.y, node.z, tostring(info.grounded)))
    end
  end

  local px, py, pz = pod_world(node)
  local rx, up, rz = basis(node)
  -- Ship frame: nose = stack axis (+Y), fwd = −Z column, right = fwd × nose.
  local nx, ny, nz = up.x, up.y, up.z
  local fx, fy, fz = -rz.x, -rz.y, -rz.z

  -- ---- board / exit -------------------------------------------------------
  if input.pressed("f") and astronaut then
    if piloting then
      piloting = false
      throttle = 0.0
      astronaut.x = px + rx.x * 2.2 + up.x * 0.4
      astronaut.y = py + rx.y * 2.2 + up.y * 0.4
      astronaut.z = pz + rx.z * 2.2 + up.z * 0.4
      if info then
        astronaut.vx, astronaut.vy, astronaut.vz = info.vel.x, info.vel.y, info.vel.z
      end
      astronaut.visible = true
      set_hud(nil)
      set_navball(false)
    elseif distance(astronaut, vec3(px, py, pz)) <= params.board_range then
      piloting = true
      astronaut.visible = false
      set_navball(true)
      -- Board in a stability hold — never inherit a velocity-pointing mode
      -- into a takeoff (a vertical climb's "retrograde" points at the core).
      sas_mode, sas_last = "stability", "stability"
      hud_t = -10.0 -- repaint the HUD immediately
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
    if astronaut and astronaut.visible
      and distance(astronaut, vec3(px, py, pz)) <= params.board_range + 1.2 then
      draw.ring(px, py, pz, up.x, up.y, up.z, 0.75, 0.3, 0.95, 1.0, 0.9)
      set_prompt("F — board")
    else
      set_prompt("")
    end
    return
  end
  set_prompt("")
  if not info then
    set_hud(part_total > 0 and string.format("assembling…  waiting for %d parts", part_total) or nil)
    return
  end

  -- Assembly vessels fly REALTIME (no warp coasting for compounds yet): if
  -- anything engaged time-warp while we're aboard, drop it — at warp the
  -- planet's rails outrun realtime physics and the ship gets left behind.
  if space.warp() > 1.001 then
    space.warp(1)
    if not warp_note then
      warp_note = true
      log("time-warp is not available aboard a built vessel yet — dropped to 1×")
    end
  end

  -- ---- throttle + pooled fuel --------------------------------------------
  if input.key("shift") then throttle = throttle + params.throttle_rate * dt end
  if input.key("ctrl") then throttle = throttle - params.throttle_rate * dt end
  if input.key("x") then throttle = 0.0 end
  if input.key("z") then throttle = 1.0 end
  throttle = math.max(0.0, math.min(1.0, throttle))
  if fuel <= 0.0 then throttle = 0.0 end
  local burn_total = 0.0
  for _, e in ipairs(engines) do burn_total = burn_total + e.burn end
  local refueling = false
  if info.anchored and fuel < fuel_cap then
    fuel = math.min(fuel_cap, fuel + params.refuel_rate * dt)
    refueling = true
  elseif throttle > 0 and not info.anchored then
    fuel = math.max(0.0, fuel - throttle * burn_total * dt)
  end

  -- ---- thrust at every live engine's offset ------------------------------
  local total_thrust = 0.0
  for _, e in ipairs(engines) do total_thrust = total_thrust + e.thrust end
  if throttle > 0 and not info.anchored and fuel > 0 then
    for _, e in ipairs(engines) do
      local ex = node.x + rx.x * e.x + up.x * e.y + rz.x * e.z
      local ey = node.y + rx.y * e.x + up.y * e.y + rz.y * e.z
      local ez = node.z + rx.z * e.x + up.z * e.y + rz.z * e.z
      local f = e.thrust * throttle
      assembly.forceAt(node, vec3(nx * f, ny * f, nz * f), vec3(ex, ey, ez))
    end
  end

  -- ---- attitude: rate-commanded, the KSP feel ----------------------------
  if input.pressed("t") then
    sas_mode = (sas_mode ~= "off") and "off" or sas_last
  end
  for k, m in pairs(SAS_KEYS) do
    if input.pressed(k) then setSAS(m) end
  end
  local p = (input.key("w") and 1 or 0) - (input.key("s") and 1 or 0)
  local y = (input.key("a") and 1 or 0) - (input.key("d") and 1 or 0)
  local r = (input.key("e") and 1 or 0) - (input.key("q") and 1 or 0)
  local rgx, rgy, rgz = cross(fx, fy, fz, nx, ny, nz) -- ship right
  local w = info.angVel
  local manual = (p ~= 0 or y ~= 0 or r ~= 0)
  local sas_on = sas_mode ~= "off"
  local sasm_x, sasm_y, sasm_z
  -- Desired world angular velocity, decomposed on the ship basis.
  local dwx, dwy, dwz
  if manual or sas_mode == "stability" or sas_mode == "off" then
    -- Current rates on the ship axes (so released axes can persist SAS-off).
    local cur_p = w.x * rgx + w.y * rgy + w.z * rgz
    local cur_y = w.x * fx + w.y * fy + w.z * fz
    local cur_r = w.x * nx + w.y * ny + w.z * nz
    local hold = sas_on and 0 or nil
    local want_p = p ~= 0 and p * params.max_rate or (hold or cur_p)
    local want_y = y ~= 0 and y * params.max_rate or (hold or cur_y)
    local want_r = r ~= 0 and r * params.max_rate or (hold or cur_r)
    if not manual and sas_mode == "off" then
      want_p, want_y, want_r = cur_p, cur_y, cur_r
    end
    dwx = rgx * want_p + fx * want_y + nx * want_r
    dwy = rgy * want_p + fy * want_y + ny * want_r
    dwz = rgz * want_p + fz * want_y + nz * want_r
  else
    -- Pointing autopilot: rotate the nose toward the target with the KSP
    -- braking law (fastest rate that can still stop on arrival).
    local db = space.body(space.dominant(node.x, node.y, node.z))
    local tx, ty, tz = sas_target_dir(node, db, sas_mode)
    if tx then
      sasm_x, sasm_y, sasm_z = tx, ty, tz
      local axx, axy, axz = cross(nx, ny, nz, tx, ty, tz)
      local s = math.sqrt(axx * axx + axy * axy + axz * axz)
      local c = nx * tx + ny * ty + nz * tz
      if s > 1e-5 then
        local theta = math.atan2(s, c)
        local rate = math.min(params.max_rate, math.sqrt(2 * params.rate_accel * theta))
        dwx, dwy, dwz = (axx / s) * rate, (axy / s) * rate, (axz / s) * rate
      else
        dwx, dwy, dwz = 0, 0, 0
      end
    else
      dwx, dwy, dwz = 0, 0, 0
    end
  end
  if not info.anchored then
    assembly.torque(node, vec3(
      (dwx - w.x) * params.torque,
      (dwy - w.y) * params.torque,
      (dwz - w.z) * params.torque
    ))
  end

  -- ---- SPACE: clamps first, then stages ----------------------------------
  if input.pressed("space") then
    if info.anchored then
      if assembly_ready(info) then
        assembly.setAnchored(node, false)
        log("launch clamps released")
      else
        log(string.format("clamps hold: %d / %d parts assembled", #info.parts, part_total))
      end
    elseif #decouplers > 0 then
      local dec = table.remove(decouplers, 1)
      local parts_nodes = {}
      for _, child in ipairs(node:children()) do
        if child.y <= dec.y + 0.01 then parts_nodes[#parts_nodes + 1] = child end
      end
      if #parts_nodes > 0 then
        drop_below(dec.y)
        local n_away = #parts_nodes
        assembly.split(node, parts_nodes, function(stage)
          local si = assembly.info(stage)
          if si then
            assembly.impulseAt(stage, vec3(nx * -3, ny * -3, nz * -3), si.com)
          end
          log("stage away: " .. n_away .. " parts")
        end)
      end
    end
  end

  update_navball(node, nx, ny, nz, fx, fy, fz, sasm_x, sasm_y, sasm_z)

  -- ---- HUD (10 Hz, the scout ship's format) ------------------------------
  if time - hud_t >= 0.1 then
    hud_t = time
    local dom = space.dominant(node.x, node.y, node.z)
    local b = dom and space.body(dom)
    local lines = {}
    local bars = math.floor(throttle * 10 + 0.5)
    local twr = (info.mass > 0) and total_thrust / (info.mass * 9.81) or 0
    lines[1] = string.format("THR [%s%s] %3d%%   SAS %s   TWR %.2f%s%s",
      string.rep("|", bars), string.rep("·", 10 - bars), throttle * 100,
      SAS_LABEL[sas_mode] or "?", twr,
      info.anchored and "   CLAMPED" or (info.grounded and "   LANDED" or ""),
      #decouplers > 0 and string.format("   STAGES %d", #decouplers) or "")
    if fuel_cap > 0 then
      local fbars = math.floor(fuel / fuel_cap * 10 + 0.5)
      local ftag = ""
      if refueling then ftag = "  REFUELING"
      elseif fuel <= 0.0 then ftag = "  ⚠ TANK EMPTY" end
      lines[2] = string.format("FUEL [%s%s] %3d%%%s",
        string.rep("|", fbars), string.rep("·", 10 - fbars), fuel / fuel_cap * 100, ftag)
    end
    if b then
      local dxr, dyr, dzr = node.x - b.x, node.y - b.y, node.z - b.z
      local rlen = math.sqrt(dxr * dxr + dyr * dyr + dzr * dzr)
      local upx, upy, upz = dxr / rlen, dyr / rlen, dzr / rlen
      local spd = math.sqrt(node.vx ^ 2 + node.vy ^ 2 + node.vz ^ 2)
      local vsp = node.vx * upx + node.vy * upy + node.vz * upz
      local nose_deg = math.deg(math.asin(math.max(-1, math.min(1,
        nx * upx + ny * upy + nz * upz))))
      lines[#lines + 1] = string.format("ALT %6.0f   SPD %6.1f   VSPD %+6.1f   NOSE %+3.0f°",
        rlen - b.radius, spd, vsp, nose_deg)
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
    if info.anchored then
      if assembly_ready(info) then
        lines[#lines + 1] = "▶ SPACE — release launch clamps"
      else
        lines[#lines + 1] = string.format("assembling…  %d / %d parts", #info.parts, part_total)
      end
    end
    lines[#lines + 1] =
      "F exit·Shift/Ctrl thr·X cut·Z full·WASD/QE rotate·T SAS·1-7 hold modes·SPACE stage·M map"
    set_hud(table.concat(lines, "\n"))
  end
end
