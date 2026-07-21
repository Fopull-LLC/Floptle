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
--   . / ,    time-warp up / down (KSP rules: coasting or parked only; any
--            stick input drops to 1×; the vessel rides exact Kepler rails)
--   M        map (focus + trajectories follow THIS craft while piloted)
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
-- Where the camera should orbit: the capsule. podL* is the pod's LOCAL
-- offset — planet_camera composes it with the node's RENDERED pose in its
-- own pass, so the orbit center sits on the ship the frame actually draws
-- (a fixedUpdate world position lags the rails carry: offset + jitter).
-- focusX/focusHeight stay published for other readers.
podLX, podLY, podLZ = 0, 1.2, 0
focusX, focusY, focusZ = nil, nil, nil
focusHeight = 1.2
sas_mode = "stability"
local sas_last = "stability"

local bp = nil
local launch_mode = false -- arrived via the builder's LAUNCH (clamp lifecycle)
local released = false    -- the pilot has released the clamps at least once
local info_seen = false   -- first sighting of the live compound (diagnostics)
local no_info_t = 0.0     -- how long we've piloted WITHOUT a compound
local engines = {}     -- { {x,y,z, dx,dy,dz, thrust, burn, branch} } vessel-local, live only
local tanks = {}       -- { {y, fuel, branch} } vessel-local, live only
local decouplers = {}  -- AXIAL cuts { {uid, y} } sorted bottom-up, fired in order
local boosters = {}    -- radial-decoupler branches { {uid, x,y,z, uids={...}} }
local pod = { x = 0, y = 0.5, z = 0 }
-- Damage model (assembly.impacts → per-part strength): see the block after
-- the staging helpers.
local bp_by_uid = {}   -- uid → blueprint part record
local part_hp = {}     -- uid → 1.0 pristine … 0.0 destroyed
local destroyed = false -- the POD is gone: the vessel is lost
local smoke_t = 0.0
local part_total = 0
local fuel_cap = 0.0
local boarding = false
local astronaut, hud, hud_prompt
local hud_t = -10.0
local navball, tape_spd, tape_alt, txt_spd, txt_alt, txt_hdg
local stage_node
-- KSP-style warp ladder (matches the scout's).
local WARP_STEPS = { 1, 5, 10, 50, 100, 1000, 10000 }

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
-- The part's local +Y (its thrust/stack axis) in the VESSEL frame, from the
-- blueprint's YXZ rotation — a sideways-mounted engine pushes sideways.
local function part_axis(d)
  local cy, sy = math.cos(d.yaw or 0), math.sin(d.yaw or 0)
  local cx, sx = math.cos(d.pitch or 0), math.sin(d.pitch or 0)
  local cz, sz = math.cos(d.roll or 0), math.sin(d.roll or 0)
  return -cy * sz + sy * sx * cz, cx * cz, sy * sz + cy * sx * cz
end

local function load_bp()
  bp = save.get("shipyard.blueprint")
  engines, tanks, decouplers, boosters, part_total, fuel_cap = {}, {}, {}, {}, 0, 0.0
  if not bp or not bp.parts then return end
  -- Parent links → radial-decoupler BRANCHES (each = the decoupler + its
  -- whole outboard subtree; they separate laterally as one group).
  local by_uid, kids = {}, {}
  for _, d in pairs(bp.parts) do
    by_uid[d.uid] = d
    local pu = d.parent or 0
    if pu ~= 0 then
      kids[pu] = kids[pu] or {}
      kids[pu][#kids[pu] + 1] = d.uid
    end
  end
  local in_branch = {} -- uid → branch root uid
  for _, d in pairs(bp.parts) do
    if (d.radial or 0) == 1 and (d.decouple or 0) == 1 then
      local uids, queue = {}, { d.uid }
      while #queue > 0 do
        local u = table.remove(queue)
        if not uids[u] then
          uids[u] = true
          in_branch[u] = d.uid
          for _, k in ipairs(kids[u] or {}) do queue[#queue + 1] = k end
        end
      end
      local pos = {}
      for u in pairs(uids) do
        local dd = by_uid[u]
        if dd then pos[#pos + 1] = { x = dd.x, y = dd.y, z = dd.z } end
      end
      boosters[#boosters + 1] = { uid = d.uid, x = d.x, y = d.y, z = d.z,
                                  uids = uids, pos = pos }
    end
  end
  for _, d in pairs(bp.parts) do
    part_total = part_total + 1
    if (d.thrust or 0) > 0 then
      local ax, ay, az = part_axis(d)
      engines[#engines + 1] = { x = d.x, y = d.y, z = d.z, dx = ax, dy = ay, dz = az,
                                thrust = d.thrust, burn = d.burn or 1,
                                branch = in_branch[d.uid], uid = d.uid }
    end
    if (d.fuel or 0) > 0 then
      tanks[#tanks + 1] = { y = d.y, fuel = d.fuel, branch = in_branch[d.uid], uid = d.uid }
      fuel_cap = fuel_cap + d.fuel
    end
    if (d.decouple or 0) == 1 and (d.radial or 0) ~= 1 then
      decouplers[#decouplers + 1] = { uid = d.uid, y = d.y }
    end
    if d.kind == "crewed" then pod = { x = d.x, y = d.y, z = d.z } end
  end
  table.sort(decouplers, function(a, b) return a.y < b.y end)
  fuel = fuel_cap
  focusHeight = pod.y
  podLX, podLY, podLZ = pod.x, pod.y, pod.z
  -- Damage model state: every part starts pristine.
  bp_by_uid = by_uid
  part_hp = {}
  for _, d in pairs(bp.parts) do part_hp[d.uid] = 1.0 end
  destroyed = false
end


-- Is this engine part of the CURRENTLY BURNING set? Bottom live axial stage
-- + every still-attached side booster (boosters light with stage 1 and burn
-- until their radial decoupler fires).
local function engine_active(e, cut)
  if e.branch then return true end -- attached booster (dropped at separation)
  return e.y < cut - 0.01
end

-- The PHYSICS-FRESH base of the vessel frame. Inside fixedUpdate the node
-- transform lags this tick's rails carry (the planet moved ~1.5 u since the
-- node was last written) — using node coords put thrust ~a hull-width off the
-- real engines (a constant spurious torque the SAS fought forever), and drew
-- boarding rings beside the ship. `info.origin` is the sim's own anchor.
local function base_of(node, info)
  if info and info.origin then
    return info.origin.x, info.origin.y, info.origin.z
  end
  return node.x, node.y, node.z
end

local function pod_world(node, info)
  local rx, up, rz = basis(node)
  local bx, by, bz = base_of(node, info)
  return bx + rx.x * pod.x + up.x * pod.y + rz.x * pod.z,
         by + rx.y * pod.x + up.y * pod.y + rz.y * pod.z,
         bz + rx.z * pod.x + up.z * pod.y + rz.z * pod.z
end

-- Boarding reach: distance to the vessel's SPINE (base → pod), not to the pod
-- point itself — a pod on top of a tall stack is 6+ units off the ground, and
-- demanding you touch it made re-boarding physically impossible. Standing at
-- the rocket counts as climbing the ladder.
local function board_dist(node, a, info)
  local px, py, pz = pod_world(node, info)
  local sx, sy, sz = base_of(node, info) -- spine base (stack bottom)
  local dx, dy, dz = px - sx, py - sy, pz - sz
  local len2 = dx * dx + dy * dy + dz * dz
  local t = 0.0
  if len2 > 1e-9 then
    t = ((a.x - sx) * dx + (a.y - sy) * dy + (a.z - sz) * dz) / len2
    t = math.max(0.0, math.min(1.0, t))
  end
  local cx, cy, cz = sx + dx * t, sy + dy * t, sz + dz * t
  return math.sqrt((a.x - cx) ^ 2 + (a.y - cy) ^ 2 + (a.z - cz) ^ 2)
end

-- ── instruments (the scout ship's cluster, reused verbatim) ─────────────────
local function set_hud(text)
  if not hud then hud = find("Ship HUD Text") end
  if not hud then return end
  local el = hud:getcomponent("UiElement")
  if el then el.visible = text ~= nil end
  if text then hud.text = text end
end

-- The FLIGHT STAGE LIST (left side): every remaining stage bottom-up with
-- its engines, the ACTIVE one marked — SPACE fires the next separation.
local stage_last = nil
local function set_stage_list(text)
  if not stage_node then stage_node = find("Stage List") end
  if not stage_node then return end
  local el = stage_node:getcomponent("UiElement")
  if el then el.visible = text ~= nil end
  if text and text ~= stage_last then
    stage_node.text = text
    stage_last = text
  end
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

-- ── engine plumes ───────────────────────────────────────────────────────────
-- Every live engine part carries an "Engine Flame" child (Flame vfx + point
-- light); the plume density and light follow the throttle. Staged-away
-- engines re-root under the detached stage, so `node:children()` only ever
-- yields the LIVE stack — no bookkeeping.
local function set_flames(node, pct, cut)
  local on = pct > 0.02
  cut = cut or math.huge
  for _, c in ipairs(node:children()) do
    -- Only ACTIVE engines light: match the child to its blueprint engine by
    -- local pose (bottom live stage + attached side boosters burn; parts
    -- above the next decoupler are a later stage, cold until their turn).
    if c.name and c.name:find("PartEngine") then
      local live = false
      if on then
        for _, e in ipairs(engines) do
          if math.abs((c.x or 0) - e.x) < 0.05 and math.abs((c.y or 0) - e.y) < 0.05
            and math.abs((c.z or 0) - e.z) < 0.05 and engine_active(e, cut) then
            live = true
            break
          end
        end
      end
      for _, f in ipairs(c:children()) do
        if f.name == "Engine Flame" then
          local ps = f:particles()
          if ps then
            if live and not ps:isPlaying() then ps:play() end
            if not live and ps:isPlaying() then ps:stop() end
            if live then ps:setIntensity(0.25 + pct * 1.25) end
          end
          local light = f:getcomponent("PointLight")
          if light then light.intensity = live and (0.8 + pct * 4.0) or 0.0 end
        end
      end
    end
  end
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
-- of the pooled fuel and forgets its engines. (Attached side boosters live
-- in their own branches — an axial cut never takes them.)
local function drop_below(cut_y)
  local frac = (fuel_cap > 0) and (fuel / fuel_cap) or 0
  local keep_e, keep_t, cap = {}, {}, 0.0
  for _, e in ipairs(engines) do
    if e.branch or e.y > cut_y + 0.01 then keep_e[#keep_e + 1] = e end
  end
  for _, t in ipairs(tanks) do
    if t.branch or t.y > cut_y + 0.01 then
      keep_t[#keep_t + 1] = t
      cap = cap + t.fuel
    end
  end
  engines, tanks, fuel_cap = keep_e, keep_t, cap
  fuel = cap * frac
end

-- Booster separation bookkeeping: the branch's engines and tanks leave with
-- it (its tanks take their pooled-fuel share along).
local function drop_branch(root_uid)
  local frac = (fuel_cap > 0) and (fuel / fuel_cap) or 0
  local keep_e, keep_t, cap = {}, {}, 0.0
  for _, e in ipairs(engines) do
    if e.branch ~= root_uid then keep_e[#keep_e + 1] = e end
  end
  for _, t in ipairs(tanks) do
    if t.branch ~= root_uid then
      keep_t[#keep_t + 1] = t
      cap = cap + t.fuel
    end
  end
  engines, tanks, fuel_cap = keep_e, keep_t, cap
  fuel = cap * frac
end

-- ── damage / destruction ────────────────────────────────────────────────────
-- `assembly.impacts()` reports how hard each PART hit something last tick
-- (the engine attributes every contact's impulse to the part that took it).
-- Under ~half the part's strength a hit is just a landing; past that, damage
-- accumulates (the part smoulders); a full-strength blow — or worn-out HP —
-- BREAKS the part: explosion, the part shears off as tumbling wreckage, and
-- losing the pod loses the ship.
local STRENGTH = { crewed = 26, tank = 10, engine = 16, structural = 22, canvas = 6 }
local function part_strength(d)
  if (d.legs or 0) == 1 then return 40 end -- legs exist to hit the ground
  return STRENGTH[d.kind or "structural"] or 18
end

-- The live child node (and blueprint uid) behind a reported part entity id.
local function uid_of_child(node, eid)
  for _, c in ipairs(node:children()) do
    if c.id == eid then
      for uid, d in pairs(bp_by_uid) do
        if math.abs((c.x or 0) - d.x) < 0.05 and math.abs((c.y or 0) - d.y) < 0.05
          and math.abs((c.z or 0) - d.z) < 0.05 then
          return uid, c
        end
      end
    end
  end
  return nil
end

-- A destroyed part's engines/tanks leave the pools (its fuel share with it).
local function drop_part(uid)
  local frac = (fuel_cap > 0) and (fuel / fuel_cap) or 0
  local keep_e, keep_t, cap = {}, {}, 0.0
  for _, e in ipairs(engines) do
    if e.uid ~= uid then keep_e[#keep_e + 1] = e end
  end
  for _, t in ipairs(tanks) do
    if t.uid ~= uid then
      keep_t[#keep_t + 1] = t
      cap = cap + t.fuel
    end
  end
  engines, tanks, fuel_cap = keep_e, keep_t, cap
  fuel = cap * frac
end

local function break_part(node, uid, d, c, hx, hy, hz)
  part_hp[uid] = 0
  spawnEffect("Explosion", hx, hy, hz)
  drop_part(uid)
  log("💥 " .. (d.label or d.id or "part") .. " destroyed!")
  if d.kind == "crewed" then
    -- The POD is the ship. Its pilot is thrown clear of the wreck.
    destroyed = true
    if piloting then
      piloting = false
      if astronaut then
        astronaut.visible = true
        astronaut.x, astronaut.y, astronaut.z = hx, hy + 2.0, hz
        local i2 = assembly.info(node)
        if i2 then
          astronaut.vx, astronaut.vy, astronaut.vz = i2.vel.x, i2.vel.y + 3.0, i2.vel.z
        end
      end
      set_hud(nil)
      set_navball(false)
      set_stage_list(nil)
      set_prompt("")
    end
    log("the pod is gone — vessel lost")
    return
  end
  -- The broken part shears off as its own tumbling wreck, kicked outward
  -- from the impact point.
  local i2 = assembly.info(node)
  if c and c.valid and i2 and #i2.parts > 1 then
    assembly.split(node, { c }, function(junk)
      local ji = assembly.info(junk)
      if ji then
        local dxk, dyk, dzk = ji.com.x - hx, ji.com.y - hy, ji.com.z - hz
        local l = math.sqrt(dxk * dxk + dyk * dyk + dzk * dzk)
        if l < 1e-3 then dxk, dyk, dzk, l = 0, 1, 0, 1 end
        assembly.impulseAt(junk,
          vec3(dxk / l * 4, dyk / l * 4 + 2, dzk / l * 4), ji.com)
      end
    end)
  end
end

local function damage_tick(node, info)
  for _, h in ipairs(assembly.impacts(node)) do
    local uid, c = uid_of_child(node, h.part)
    local d = uid and bp_by_uid[uid]
    local hp = uid and part_hp[uid]
    if d and hp and hp > 0 then
      local s = part_strength(d)
      if h.impulse >= s * 0.45 then
        local dmg = math.min(1.0, (h.impulse - s * 0.45) / (s * 0.55))
        part_hp[uid] = hp - dmg
        if part_hp[uid] <= 0 then
          break_part(node, uid, d, c, h.x, h.y, h.z)
        elseif dmg > 0.04 then
          spawnEffect("Smoke", h.x, h.y, h.z)
          log(string.format("⚠ %s damaged — %d%% left",
            d.label or d.id or "part", part_hp[uid] * 100))
        end
      end
    end
  end
  -- Damaged parts SMOULDER: a smoke puff re-anchored to the part each beat,
  -- so the trail follows the flying (or crashed) ship.
  if time - smoke_t > 0.7 then
    smoke_t = time
    local rx, up, rz = basis(node)
    local bx, by, bz = base_of(node, info)
    for uid, hp in pairs(part_hp) do
      if hp > 0 and hp < 0.65 then
        local d = bp_by_uid[uid]
        if d then
          spawnEffect("Smoke",
            bx + rx.x * d.x + up.x * d.y + rz.x * d.z,
            by + rx.y * d.x + up.y * d.y + rz.y * d.z,
            bz + rx.z * d.x + up.z * d.y + rz.z * d.z)
        end
      end
    end
  end
end


-- ── lifecycle ───────────────────────────────────────────────────────────────
function start(node)
  load_bp()
  if (save.get("shipyard.pilot") or 0) == 1 then
    save.set("shipyard.pilot", 0)
    boarding = true
    launch_mode = true
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

  -- State diagnostics + clamp redundancy: the moment the compound exists,
  -- say so — and on a builder launch, ENGAGE the clamps from this side too
  -- (belt and braces: a lost spawner callback must not leave the vessel
  -- loose on the pad). A piloted vessel with no compound for seconds is an
  -- impossible state worth shouting about.
  if info and not info_seen then
    info_seen = true
    if launch_mode and not released and not info.anchored then
      assembly.setAnchored(node, true)
      log("vessel: clamps engaged (" .. #info.parts .. " parts)")
    else
      log("vessel: compound live (" .. #info.parts .. " parts)")
    end
  end
  if piloting and not info then
    no_info_t = no_info_t + dt
    if no_info_t > 5.0 and no_info_t - dt <= 5.0 then
      log("vessel: PILOTED BUT NO PHYSICS ASSEMBLY under this root for 5s — " ..
        "the launch cannot proceed; please report this line")
    end
  else
    no_info_t = 0.0
  end

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

  -- ---- damage: what did each part hit last tick, and how hard? -----------
  -- Runs piloted OR derelict — a parked ship hit by falling wreckage breaks
  -- all the same. (While clamped/anchored the sim makes no contacts.)
  if info then damage_tick(node, info) end
  if destroyed then
    set_prompt("")
    return -- a podless derelict: nothing left to board or fly
  end

  local px, py, pz = pod_world(node, info)
  local bx, by, bz = base_of(node, info)
  local rx, up, rz = basis(node)
  -- Ship frame: nose = stack axis (+Y), fwd = −Z column, right = fwd × nose.
  local nx, ny, nz = up.x, up.y, up.z
  local fx, fy, fz = -rz.x, -rz.y, -rz.z
  -- The camera's orbit center: the CAPSULE's live world position (the root
  -- node sits at the stack base, and gravity-up offsets drift off the hull
  -- the moment the vessel pitches — world-exact is the only stable center).
  focusX, focusY, focusZ = px, py, pz
  -- Only the BOTTOM live stage burns: everything above the next decoupler
  -- waits its turn. No decouplers left = every remaining engine fires.
  local cut = decouplers[1] and decouplers[1].y or math.huge

  -- ---- board / exit -------------------------------------------------------
  if input.pressed("f") and astronaut then
    if piloting then
      piloting = false
      throttle = 0.0
      set_flames(node, 0)
      astronaut.x = px + rx.x * 2.2 + up.x * 0.4
      astronaut.y = py + rx.y * 2.2 + up.y * 0.4
      astronaut.z = pz + rx.z * 2.2 + up.z * 0.4
      if info then
        astronaut.vx, astronaut.vy, astronaut.vz = info.vel.x, info.vel.y, info.vel.z
      end
      astronaut.visible = true
      set_hud(nil)
      set_navball(false)
      set_stage_list(nil)
    elseif board_dist(node, astronaut, info) <= params.board_range then
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
    -- (The hatch RING is drawn in lateUpdate — the camera pass — where the
    -- node pose matches what this frame renders; drawing it here put it a
    -- rails-carry beside the visible ship.)
    if astronaut and astronaut.visible
      and board_dist(node, astronaut, info) <= params.board_range + 1.4 then
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

  -- ---- the map owns the keyboard while it's open -------------------------
  -- In map mode WASD/QE tune the planned burn, X zeroes it, arrows slide it,
  -- SPACE is free for the planner — flying the ship (or STAGING) with those
  -- same keys mid-planning would be chaos. The SAS keeps holding attitude;
  -- throttle, stick, stage and warp keys stand down until the map closes.
  local sc_map = findScript("ship_controller")
  local map_open = (sc_map and sc_map.map_view) or false

  -- ---- time warp (compounds coast on their own Kepler rails now) ---------
  local warp = space.warp()
  if not map_open and (input.pressed(".") or input.pressed(",")) then
    local dir = input.pressed(".") and 1 or -1
    local idx = 1
    for i, w in ipairs(WARP_STEPS) do
      if warp >= w - 0.5 then idx = i end
    end
    local nxt = WARP_STEPS[math.max(1, math.min(#WARP_STEPS, idx + dir))]
    if nxt > warp then
      -- KSP rules: no thrust, and either parked or high enough that the
      -- conic can't clip terrain this instant.
      local alt_ok = info.grounded or info.anchored
      local d0 = space.dominant(node.x, node.y, node.z)
      local b0 = d0 and space.body(d0)
      if not alt_ok and b0 then
        local rr = math.sqrt((bx - b0.x) ^ 2 + (by - b0.y) ^ 2 + (bz - b0.z) ^ 2)
        alt_ok = rr - b0.radius > 40.0
      end
      if throttle > 0.01 then
        log("warp locked: cut throttle first")
        nxt = warp
      elseif not alt_ok then
        log("warp locked: too low")
        nxt = warp
      end
    end
    if nxt ~= warp then
      space.warp(nxt)
      warp = nxt
    end
  end
  -- Hands on the stick cancel warp — the rails own the ship up there.
  -- (Not while the map is open: there WASD are the burn-planning keys.)
  if warp > 1.001 and not map_open then
    local touched = input.key("shift") or input.key("ctrl") or input.key("z")
      or input.key("w") or input.key("a") or input.key("s") or input.key("d")
      or input.key("q") or input.key("e")
    if touched then
      space.warp(1)
      warp = 1
    end
  end

  -- ---- throttle + pooled fuel --------------------------------------------
  if not map_open then
    if input.key("shift") then throttle = throttle + params.throttle_rate * dt end
    if input.key("ctrl") then throttle = throttle - params.throttle_rate * dt end
    if input.key("x") then throttle = 0.0 end
    if input.key("z") then throttle = 1.0 end
  end
  throttle = math.max(0.0, math.min(1.0, throttle))
  if fuel <= 0.0 then throttle = 0.0 end
  local burn_total = 0.0
  for _, e in ipairs(engines) do
    if engine_active(e, cut) then burn_total = burn_total + e.burn end
  end
  local refueling = false
  if info.anchored and fuel < fuel_cap then
    fuel = math.min(fuel_cap, fuel + params.refuel_rate * dt)
    refueling = true
  elseif throttle > 0 and not info.anchored then
    fuel = math.max(0.0, fuel - throttle * burn_total * dt)
  end

  -- ---- thrust at every ACTIVE engine's offset, along ITS OWN axis --------
  local total_thrust = 0.0
  for _, e in ipairs(engines) do
    if engine_active(e, cut) then total_thrust = total_thrust + e.thrust end
  end
  if throttle > 0 and not info.anchored and fuel > 0 then
    for _, e in ipairs(engines) do
      if engine_active(e, cut) then
        -- Physics-fresh base: thrust lands ON the hull, not a rails-carry
        -- beside it (the old node-pose math was a constant torque bias).
        local ex = bx + rx.x * e.x + up.x * e.y + rz.x * e.z
        local ey = by + rx.y * e.x + up.y * e.y + rz.y * e.z
        local ez = bz + rx.z * e.x + up.z * e.y + rz.z * e.z
        -- The engine's blueprint orientation IS its thrust axis (its local
        -- +Y in the vessel frame) — a sideways engine pushes sideways, which
        -- is what makes rocket cars and lateral thrusters real.
        local wdx = rx.x * e.dx + up.x * e.dy + rz.x * e.dz
        local wdy = rx.y * e.dx + up.y * e.dy + rz.y * e.dz
        local wdz = rx.z * e.dx + up.z * e.dy + rz.z * e.dz
        local f = e.thrust * throttle
        assembly.forceAt(node, vec3(wdx * f, wdy * f, wdz * f), vec3(ex, ey, ez))
      end
    end
  end
  set_flames(node, (info.anchored or fuel <= 0) and 0 or throttle, cut)

  -- ---- attitude: rate-commanded, the KSP feel ----------------------------
  if input.pressed("t") then
    sas_mode = (sas_mode ~= "off") and "off" or sas_last
  end
  if not map_open then
    for k, m in pairs(SAS_KEYS) do
      if input.pressed(k) then setSAS(m) end
    end
  end
  local p = (input.key("w") and 1 or 0) - (input.key("s") and 1 or 0)
  local y = (input.key("a") and 1 or 0) - (input.key("d") and 1 or 0)
  local r = (input.key("e") and 1 or 0) - (input.key("q") and 1 or 0)
  if map_open then p, y, r = 0, 0, 0 end -- the stick is frozen in the map
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
  if not info.anchored and warp <= 1.001 then
    assembly.torque(node, vec3(
      (dwx - w.x) * params.torque,
      (dwy - w.y) * params.torque,
      (dwz - w.z) * params.torque
    ))
  end

  -- ---- SPACE: clamps first, then boosters, then axial stages -------------
  if input.pressed("space") and not map_open then
    if info.anchored then
      if assembly_ready(info) then
        released = true
        assembly.setAnchored(node, false)
        log("launch clamps released")
      else
        log(string.format("clamps hold: %d / %d parts assembled", #info.parts, part_total))
      end
    elseif #boosters > 0 then
      -- SIDE BOOSTERS first: every radial branch kicks away laterally at
      -- once (symmetric pairs leave together, so the ship stays balanced).
      local groups = boosters
      boosters = {}
      for _, g in ipairs(groups) do
        local parts_nodes = {}
        for _, child in ipairs(node:children()) do
          for _, q in ipairs(g.pos) do
            if math.abs((child.x or 0) - q.x) < 0.05 and math.abs((child.y or 0) - q.y) < 0.05
              and math.abs((child.z or 0) - q.z) < 0.05 then
              parts_nodes[#parts_nodes + 1] = child
              break
            end
          end
        end
        if #parts_nodes > 0 then
          drop_branch(g.uid)
          -- Kick OUTWARD: the branch's radial direction in the vessel frame.
          local ol = math.sqrt(g.x * g.x + g.z * g.z)
          local ox, oz = 1, 0
          if ol > 1e-4 then ox, oz = g.x / ol, g.z / ol end
          local kx = rx.x * ox + rz.x * oz
          local ky = rx.y * ox + rz.y * oz
          local kz = rx.z * ox + rz.z * oz
          local n_away = #parts_nodes
          assembly.split(node, parts_nodes, function(stage)
            local si = assembly.info(stage)
            if si then
              assembly.impulseAt(stage, vec3(kx * 4, ky * 4, kz * 4), si.com)
            end
            log("boosters away: " .. n_away .. " parts")
          end)
        end
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
    -- The map owns the screen while it's open (its info panel replaces the
    -- flight HUD) — stand down instead of painting through it.
    if map_open then
      set_hud(nil)
      set_stage_list(nil) -- the list must not stay painted over the map
      return
    end
    local dom = space.dominant(node.x, node.y, node.z)
    local b = dom and space.body(dom)
    local lines = {}
    local bars = math.floor(throttle * 10 + 0.5)
    local twr = (info.mass > 0) and total_thrust / (info.mass * 9.81) or 0
    lines[1] = string.format("THR [%s%s] %3d%%   SAS %s   TWR %.2f%s%s%s",
      string.rep("|", bars), string.rep("·", 10 - bars), throttle * 100,
      SAS_LABEL[sas_mode] or "?", twr,
      info.anchored and "   CLAMPED" or (info.grounded and "   LANDED" or ""),
      (#decouplers + (#boosters > 0 and 1 or 0)) > 0
        and string.format("   STAGES %d", #decouplers + (#boosters > 0 and 1 or 0)) or "",
      warp > 1.001 and string.format("   ⏩ WARP %d×", warp) or "")
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
    -- Damage report: the worst-off surviving part (the smoke tells you where).
    local worst_uid, worst_hp
    for uid, hp in pairs(part_hp) do
      if hp > 0 and hp < 1 and (not worst_hp or hp < worst_hp) then
        worst_uid, worst_hp = uid, hp
      end
    end
    if worst_hp then
      local d = bp_by_uid[worst_uid]
      lines[#lines + 1] = string.format("⚠ DAMAGE  %s %d%%",
        (d and (d.label or d.id)) or "part", worst_hp * 100)
    end
    lines[#lines + 1] =
      "F exit·Shift/Ctrl thr·X cut·Z full·WASD/QE rotate·T SAS·1-7 hold·SPACE stage·./, warp·M map"
    set_hud(table.concat(lines, "\n"))

    -- The stage list (right edge, ROCKET ORDER): drawn top-down like the
    -- vehicle itself — upper stages first, the stage that burns NOW at the
    -- bottom marked ▶. Matches the flight model exactly (the window between
    -- consecutive decouplers); rows fall off the bottom as you stage.
    local sl = { "STAGES — SPACE fires next", "" }
    for k = #decouplers, 0, -1 do
      local lo = (k == 0) and -math.huge or decouplers[k].y
      local hi = decouplers[k + 1] and decouplers[k + 1].y or math.huge
      local n_eng, th = 0, 0
      for _, e in ipairs(engines) do
        if not e.branch and e.y > lo + 0.01 and e.y < hi - 0.01 then
          n_eng = n_eng + 1
          th = th + e.thrust
        end
      end
      local tag = (k == 0 and #boosters == 0) and "  ▶" or ""
      if n_eng > 0 then
        sl[#sl + 1] = string.format("S%d   %d engine%s   %d kN%s",
          k + 1, n_eng, n_eng == 1 and "" or "s", th, tag)
      else
        sl[#sl + 1] = string.format("S%d   (no engines)%s", k + 1, tag)
      end
    end
    -- Side boosters separate FIRST — they sit at the bottom of the list,
    -- exactly like the rocket's silhouette.
    if #boosters > 0 then
      local n_eng, th = 0, 0
      for _, e in ipairs(engines) do
        if e.branch then
          n_eng = n_eng + 1
          th = th + e.thrust
        end
      end
      sl[#sl + 1] = string.format("BOOSTERS ×%d   %d engine%s   %d kN  ▶",
        #boosters, n_eng, n_eng == 1 and "" or "s", th)
    end
    set_stage_list(table.concat(sl, "\n"))
  end
end

-- Camera-pass drawing: the hatch ring must sit on the ship THE FRAME RENDERS
-- (lateUpdate reads post-writeback poses) — drawn from fixedUpdate it floats
-- a rails-carry beside the hull on any orbiting world.
function lateUpdate(node, dt)
  if piloting or not astronaut or not astronaut.visible then return end
  local info = assembly.info(node)
  if board_dist(node, astronaut, info) > params.board_range + 1.4 then return end
  local rx, up, rz = basis(node)
  local px = node.x + rx.x * pod.x + up.x * pod.y + rz.x * pod.z
  local py = node.y + rx.y * pod.x + up.y * pod.y + rz.y * pod.z
  local pz = node.z + rx.z * pod.x + up.z * pod.y + rz.z * pod.z
  draw.ring(px, py, pz, up.x, up.y, up.z, 0.75, 0.3, 0.95, 1.0, 0.9)
end
