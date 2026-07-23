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
-- True while we've asked the engine to keep this vessel out of distant-craft
-- LOD (assembly.keepLive) — set while flying from the map, whose camera pulls
-- far back and would otherwise freeze us on rails (dead throttle/steering).
local kept_live = false
-- Where the camera should orbit: the capsule. podL* is the pod's LOCAL
-- offset — planet_camera composes it with the node's RENDERED pose in its
-- own pass, so the orbit center sits on the ship the frame actually draws
-- (a fixedUpdate world position lags the rails carry: offset + jitter).
-- focusX/focusHeight stay published for other readers.
podLX, podLY, podLZ = 0, 1.2, 0
focusX, focusY, focusZ = nil, nil, nil
focusHeight = 1.2
sas_mode = "stability"
-- Peripheral state (published: damage tolerance + HUD read it).
gear_deployed = true
-- Current reentry-heating rate (dmg/s at the nose; published for the HUD).
heating = 0.0
local heat_fx_t = -10.0
-- Parachutes: armed in staging, deployed on their stage; while open + in
-- atmosphere they drag hard against velocity (a soft descent). Ripped off if
-- opened too fast/high (they only bite in thick air).
chutes = {}                -- { {x,y,z, uid} } from the blueprint
chutes_deployed = false    -- published for the HUD
local chute_anim = 0.0     -- 0 packed … 1 fully open (canopy grows)
local sas_last = "stability"

-- ── ship peripherals ────────────────────────────────────────────────────────
-- Landing-gear fold angles (roll radians about Z). The leg's two joints swing
-- from a STOWED pose (thigh folded up, shin jack-knifed back) to a DEPLOYED one
-- (thigh out+down, knee unfolded so the foot reaches out and down). Tuned in
-- crates/floptle-assets/examples/leg_probe.rs against the recentered meshes.
-- NOTE: if a leg folds the WRONG way, flip the STOW value (past the DEP value).
local GEAR_DEP_U, GEAR_STOW_U = 0.5, 3.0   -- upper strut (hip)
local GEAR_DEP_K, GEAR_STOW_K = 0.4, -2.4  -- lower strut (knee)
local function gear_lerp(a, b, f) return a + (b - a) * f end

-- Attached DEVICES the pilot can actuate, detected from the blueprint by
-- part capability. Each device kind carries its key, its part set and its
-- actuation (an animation applied to the live child nodes). Adding a new
-- peripheral (lights, solar panels, wheels…) is one more registry entry —
-- the detection, keybind, HUD tag and per-tick actuation all come along.
local DEVICES = {
  gear = {
    key = "g", label = "GEAR",
    detect = function(d) return (d.legs or 0) == 1 end,
    parts = {},      -- blueprint uids, filled by load_bp
    on = true,       -- legs spawn deployed (the pad handshake needs them)
    anim = 1.0,      -- 0 tucked … 1 deployed
    speed = 2.0,     -- anim units per second
    pose = {},       -- uid → cached authored child pose
    -- MULTI-JOINTED fold: each leg part is a hierarchy (PartLegs.prefab.ron) —
    -- LegUpperPivot rolls at the hip, LegKneePivot (its child) rolls at the knee.
    -- We drive BOTH so the leg folds up + jack-knifes (retracted) and swings
    -- out + down with the knee unfolding (deployed). f: 0 stowed … 1 deployed.
    -- The whole symmetry ring folds together (one device drives every leg).
    apply = function(dev, child, pose, f)
      -- Find + cache the two pivot nodes under this leg part (once per leg).
      local j = pose.joints
      if not j then
        j = {}
        for _, c in ipairs(child:children()) do
          if c.name == "LegUpperPivot" then
            j.upper = c
            for _, gc in ipairs(c:children()) do
              if gc.name == "LegKneePivot" then j.knee = gc end
            end
          end
        end
        pose.joints = j
      end
      if j.upper then j.upper.roll = gear_lerp(GEAR_STOW_U, GEAR_DEP_U, f) end
      if j.knee then j.knee.roll = gear_lerp(GEAR_STOW_K, GEAR_DEP_K, f) end
    end,
  },
  comms = {
    key = "u", label = "COMMS DISH",
    detect = function(d) return (d.comms or 0) == 1 end,
    parts = {},
    on = false,      -- dishes stow for launch, deploy in space (U)
    anim = 0.0,
    speed = 1.2,
    pose = {},
    -- Tip the dish up to point at the sky when deployed.
    apply = function(dev, child, pose, f)
      child.pitch = (pose.pitch or 0) - (1 - f) * 1.4
      child.scale_y = pose.sy * (0.5 + 0.5 * f)
    end,
  },
  -- SOLAR PANELS deploy on the SAME "U" key as the dish (utilities), so one
  -- press extends the whole telemetry-and-power kit. Stowed they fold down to a
  -- stub; deployed they unfold to full span and tip up to catch the sun. Power
  -- generation (only while deployed AND sunlit) lives in power_tick.
  solar = {
    key = "u", label = "SOLAR PANELS",
    detect = function(d) return (d.solar or 0) == 1 end,
    parts = {},
    on = false,
    anim = 0.0,
    speed = 0.8,
    pose = {},
    apply = function(dev, child, pose, f)
      child.scale_x = (pose.sx or 1.0) * (0.18 + 0.82 * f)  -- unfold the span
      child.pitch = (pose.pitch or 0) - (1 - f) * 0.5       -- tip toward the sky
    end,
  },
}

local bp = nil
local launch_mode = false -- arrived via the builder's LAUNCH (clamp lifecycle)
local released = false    -- the pilot has released the clamps at least once
local info_seen = false   -- first sighting of the live compound (diagnostics)
local no_info_t = 0.0     -- how long we've piloted WITHOUT a compound
local engines = {}     -- { {x,y,z, dx,dy,dz, thrust, burn, branch} } vessel-local, live only
local tanks = {}       -- { {y, fuel, branch} } vessel-local, live only
-- SEPARATION EVENTS in firing order (the builder's STAGING panel decides):
-- {kind="axial", uid, y} splits everything below the cut; {kind="ring",
-- branches={...}} kicks a whole booster ring away laterally. SPACE fires
-- events[1]; an axial cut prunes events that depart with the lower stack.
local events = {}
local pod = { x = 0, y = 0.5, z = 0 }
-- Damage model (assembly.impacts → per-part strength): see the block after
-- the staging helpers.
local bp_by_uid = {}   -- uid → blueprint part record
local bp_in_branch = {} -- uid → radial-branch root uid (nil = on the spine)
local bp_kids = {}     -- uid → { child uids } (the attachment tree; built in load_bp)
local pod_uid = nil    -- the crewed part's uid = the tree's keep-side anchor
local axial_set_cache = {} -- dec uid → set of uids that DEPART when it fires (memoized)
local eid_uid = {}     -- part entity id → blueprint uid (resolved once, then stable)
local part_hp = {}     -- uid → 1.0 pristine … 0.0 destroyed
local destroyed = false -- the POD is gone: the vessel is lost
local smoke_t = 0.0
local scrape_t = 0.0    -- rate-limits topple/belly-slide grinding damage
local part_total = 0
local fuel_cap = 0.0
-- ── electrical (EC) — batteries store it, solar panels make it, comms spend it ─
-- All one table so fixedUpdate spends a SINGLE upvalue on the electrical state
-- (that function is close to Lua's 60-upvalue ceiling).
local elec = {
  charge = 0.0,     -- current electric charge
  cap = 0.0,        -- total battery capacity (Σ part ec)
  gen = 0.0,        -- generation at full sun (Σ deployed-panel gen)
  comms = false,    -- carries at least one comms dish
  link = false,     -- dish deployed AND powered = telemetry live (tracker)
  ptime = 0.0,      -- rate-limits the tracker heartbeat
  sun = nil, sun_t = -10.0,
}
local boarding = false
local astronaut, hud, hud_prompt
local hud_panel, stage_panel  -- the digital backing frames the readouts sit inside
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

-- ── audio ────────────────────────────────────────────────────────────────────
-- Spatial SFX ride the world through the SFX mixer bus (a short reverb +
-- compressor there sell scale and stop a stack of engines swamping the mix);
-- the ambient drone sits on the Ambient bus. Persistent LOOPS (engine, reentry,
-- bed) keep ONE voice and ride its volume/pitch with throttle/heat instead of
-- restarting — no clicks, no voice churn. All clips are Kenney CC0.
local SFX = {
  engine   = "audio/kenney/sci-fi/spaceEngineLarge_000.ogg",
  ignite   = "audio/kenney/sci-fi/thrusterFire_000.ogg",
  stage    = "audio/kenney/impact/impactMetal_heavy_000.ogg",
  boosters = "audio/kenney/sci-fi/impactMetal_000.ogg",
  clamp    = "audio/kenney/impact/impactMetal_medium_000.ogg",
  explode  = "audio/kenney/sci-fi/explosionCrunch_000.ogg",
  boom     = "audio/kenney/sci-fi/lowFrequency_explosion_000.ogg",
  chute    = "audio/kenney/interface/open_001.ogg",
  reentry  = "audio/kenney/sci-fi/forceField_000.ogg",
  gear     = "audio/kenney/sci-fi/doorOpen_000.ogg",
}
local engine_sfx, reentry_sfx = nil, nil
local throttle_prev = 0.0
local was_grounded = false   -- for the touchdown dust puff
local launch_dust_t = -10.0  -- throttle-up ground-dust cadence
local wind_intensity = 0.0   -- 0..~1.4 air density × speed (roar + shake)
local shake_published = false -- did THIS vessel last write cam.shake? (clean 0)
-- SCREEN SHAKE: a transient magnitude (bumped by impacts/explosions, decays
-- fast) added to a continuous base (wind + ground thrust). Published each frame
-- as `cam.shake` for planet_camera to jitter the view. `add_shake` spikes it.
local shake = 0.0
local function add_shake(amount)
  shake = math.min(1.6, shake + amount)
end
-- Publish the shared cam.shake for planet_camera. `base` is the continuous
-- component (wind/thrust, piloting only). We only write when we have something
-- to say (piloting, or a live transient from an impact) so parked/other craft
-- never stomp the value; a single trailing 0 settles the view.
local function publish_shake(base)
  local total = shake + (base or 0.0)
  if (base and base > 0.0) or total > 0.01 then
    save.set("cam.shake", math.min(1.0, total))
    shake_published = true
  elseif shake_published then
    save.set("cam.shake", 0.0)
    shake_published = false
  end
end

-- A one-shot spatial hit at a world point (KSP-scale falloff, SFX bus).
local function sfx3(clip, x, y, z, vol, pitch)
  if not audio then return end
  audio.play(clip, x, y, z, {
    track = "SFX", volume = vol or 1.0, pitch = pitch or 1.0,
    mode = "spatial", falloff = "inverse", minDistance = 4.0, maxDistance = 800.0,
  })
end

-- Blast a crater where an explosion meets the ground: a real gouge PLUS a wide
-- scorched scuff so the scar reads even on a glancing blast. `sd < r * 2` widens the
-- proximity gate (a blast a little above the surface still leaves a mark), and the
-- dig is deepened so a destroyed ship visibly cracks the ground.
local function make_crater(x, y, z, r)
  local sd = terrain.query(x, y, z)
  if not sd then return end
  if sd < r * 2.0 then
    -- Scorch first (color-only, always lands within reach); then dig if genuinely in.
    terrain.paint(x, y, z, r * 1.6, 0.08, 0.07, 0.06, 0.7)
  end
  if sd < r then terrain.dig(x, y, z, r, 1.0) end
end

-- The engine LOOP follows the vessel and rides the throttle; born on the first
-- burn, muted (not stopped) when idle so there's no restart pop.
local function update_engine_audio(node, burning, thr)
  if not audio then return end
  if burning then
    if not engine_sfx then
      engine_sfx = audio.play(SFX.engine, node, {
        track = "SFX", loop = true, mode = "spatial", falloff = "inverse",
        minDistance = 6.0, maxDistance = 900.0, volume = 0.0 })
    end
    if engine_sfx then
      engine_sfx:setVolume(0.35 + thr * 0.8)
      engine_sfx:setPitch(0.82 + thr * 0.5)
    end
  elseif engine_sfx then
    engine_sfx:setVolume(0.0)
  end
end

-- The atmospheric ROAR: one loop whose volume rides the WIND (air density ×
-- speed — you hear it long before you're hot) and climbs further as reentry
-- heating builds; pitch rises with speed. This is the audible "you're going too
-- fast, slow down" cue Ty wanted, and it silences as the air thins out.
local function update_wind_audio(node, wind, flux, spd)
  if not audio then return end
  local intensity = math.max(wind, flux * 5.0) -- burning adds to the roar
  if intensity > 0.03 then
    if not reentry_sfx then
      reentry_sfx = audio.play(SFX.reentry, node, {
        track = "SFX", loop = true, mode = "spatial", falloff = "inverse",
        minDistance = 8.0, maxDistance = 1200.0, volume = 0.0 })
    end
    if reentry_sfx then
      reentry_sfx:setVolume(math.min(1.0, 0.12 + intensity * 0.9))
      reentry_sfx:setPitch(0.7 + math.min(0.9, (spd or 0) / 320.0))
    end
  elseif reentry_sfx then
    reentry_sfx:setVolume(0.0)
  end
end

-- Stop the vessel's persistent loops (EVA, breakup) — silence, don't leak.
local function silence_loops()
  if engine_sfx then engine_sfx:stop(); engine_sfx = nil end
  if reentry_sfx then reentry_sfx:stop(); reentry_sfx = nil end
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
  engines, tanks, events, part_total, fuel_cap = {}, {}, {}, 0, 0.0
  elec.cap, elec.gen, elec.comms = 0.0, 0.0, false
  local decouplers, boosters = {}, {}
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
  for _, dev in pairs(DEVICES) do
    dev.parts, dev.nodes, dev.pose = {}, {}, {}
    dev.on, dev.anim, dev.anim_applied = true, 1.0, nil
  end
  for _, d in pairs(bp.parts) do
    part_total = part_total + 1
    for _, dev in pairs(DEVICES) do
      if dev.detect(d) then dev.parts[#dev.parts + 1] = d.uid end
    end
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
    -- Electrical parts: batteries add capacity, panels add generation, a dish
    -- flags that this vessel can hold a telemetry link (feeds the tracker).
    if (d.power or 0) == 1 then elec.cap = elec.cap + (d.ec or 0) end
    if (d.solar or 0) == 1 then elec.gen = elec.gen + (d.gen or 0) end
    if (d.comms or 0) == 1 then elec.comms = true end
    if d.kind == "crewed" then pod = { x = d.x, y = d.y, z = d.z }; pod_uid = d.uid end
  end
  table.sort(decouplers, function(a, b) return a.y < b.y end)
  -- Build the ordered EVENT list: booster branches group into ring events by
  -- symmetry group (a lone radial decoupler is its own ring), axial cuts are
  -- one event each. Order = the builder's saved stages; parts without one
  -- fall back to the classic default (rings first, then axial bottom-up).
  local rings = {}
  for _, g in ipairs(boosters) do
    local dd = by_uid[g.uid]
    local key = (dd and dd.sym and dd.sym ~= 0) and ("g" .. dd.sym) or ("u" .. g.uid)
    local ev = rings[key]
    if not ev then
      ev = { kind = "ring", branches = {}, stage = 1e9, y = g.y }
      rings[key] = ev
      events[#events + 1] = ev
    end
    ev.branches[#ev.branches + 1] = g
    local st = dd and (dd.stage or 0) or 0
    if st > 0 and st < ev.stage then ev.stage = st end
    ev.y = math.min(ev.y, g.y)
  end
  for _, dec in ipairs(decouplers) do
    local dd = by_uid[dec.uid]
    local st = dd and (dd.stage or 0) or 0
    events[#events + 1] = { kind = "axial", uid = dec.uid, y = dec.y,
                            stage = st > 0 and st or 1e9 }
  end
  -- PARACHUTES: every chute part on board is armed as ONE deploy event
  -- (staging it opens all of them). Defaults LAST (you pull the chutes on
  -- the way down, after everything else has separated).
  chutes = {}
  local chute_uids = {}
  local chute_stage = 1e9
  for _, d in pairs(bp.parts) do
    if (d.chute or 0) == 1 then
      chutes[#chutes + 1] = { x = d.x, y = d.y, z = d.z, uid = d.uid }
      chute_uids[#chute_uids + 1] = d.uid
      local st = d.stage or 0
      if st > 0 and st < chute_stage then chute_stage = st end
    end
  end
  if #chutes > 0 then
    events[#events + 1] = { kind = "chute", uids = chute_uids,
                            stage = chute_stage < 1e9 and chute_stage or 2e9 }
  end
  for _, ev in ipairs(events) do
    if ev.stage >= 1e9 then
      -- default rank: rings ahead of axial, lower first; chutes dead last.
      if ev.kind == "chute" then
        ev.stage = 3e6
      else
        ev.stage = 1e6 + (ev.kind == "ring" and 0 or 1e3) + ev.y
      end
    end
  end
  table.sort(events, function(a, b) return a.stage < b.stage end)
  fuel = fuel_cap
  elec.charge = elec.cap   -- batteries roll out of the VAB topped up
  focusHeight = pod.y
  podLX, podLY, podLZ = pod.x, pod.y, pod.z
  -- Damage model state: every part starts pristine.
  bp_by_uid = by_uid
  bp_in_branch = in_branch
  bp_kids = kids            -- the attachment tree (uid → children) for staging
  axial_set_cache = {}      -- recompute departing sets for this blueprint
  eid_uid = {}              -- clear the part-entity → uid cache
  part_hp = {}
  for _, d in pairs(bp.parts) do part_hp[d.uid] = 1.0 end
  destroyed = false
  chutes_deployed = false
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
  if not hud_panel then hud_panel = find("HUD Panel") end
  -- Toggle the BACKING PANEL (the text rides inside it) so the frame appears and
  -- vanishes with the readout — never an empty box while walking around.
  local panel = hud_panel or hud
  if panel then
    local el = panel:getcomponent("UiElement")
    if el then el.visible = text ~= nil end
  end
  if hud and text then hud.text = text end
end

-- The FLIGHT STAGE LIST (left side): every remaining stage bottom-up with
-- its engines, the ACTIVE one marked — SPACE fires the next separation.
local stage_last = nil
local function set_stage_list(text)
  if not stage_node then stage_node = find("Stage List") end
  if not stage_panel then stage_panel = find("Stage Panel") end
  local panel = stage_panel or stage_node
  if panel then
    local el = panel:getcomponent("UiElement")
    if el then el.visible = text ~= nil end
  end
  if stage_node and text and text ~= stage_last then
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
-- The DEPARTING SET of an axial decoupler, by the ATTACHMENT TREE — not world
-- height. Firing a decoupler cuts the one tree edge above it (dec → its parent)
-- and the side WITHOUT the pod flies away. Concretely: the decoupler plus its
-- whole subtree (everything built onto it, away from the pod) departs; the pod
-- side stays. This is why a nose cone hung LOW on an upper stage now stays, and
-- a strut dangling DOWN from an upper stage is no longer swept off by a cut
-- below it (both were the "geometry read the wrong stage" bug). Radial parts
-- ride whichever spine part they're bolted to — the tree already encodes that.
-- Memoized per decoupler; the cache is cleared each load_bp.
local function axial_departing(dec_uid)
  local cached = axial_set_cache[dec_uid]
  if cached then return cached end
  local set, queue = { [dec_uid] = true }, { dec_uid }
  while #queue > 0 do
    local u = table.remove(queue)
    for _, k in ipairs(bp_kids[u] or {}) do
      if not set[k] then set[k] = true; queue[#queue + 1] = k end
    end
  end
  -- Robustness: if the pod ended up on the subtree side (a craft built pod-down),
  -- the OTHER side is the one that leaves — invert so the pod always stays.
  if pod_uid and set[pod_uid] then
    local inv = {}
    for u in pairs(bp_by_uid) do if not set[u] then inv[u] = true end end
    set = inv
  end
  axial_set_cache[dec_uid] = set
  return set
end

-- Does a radial booster BRANCH leave with an axial cut? Only if the spine part
-- it's bolted to (its mount = the radial decoupler's parent) is in that cut's
-- departing set. A booster on the surviving upper stack stays put; one on the
-- discarded lower stack physically has to go. Pure tree — no raw-y guessing.
local function branch_departs(root_uid, dec_uid)
  local dr = bp_by_uid[root_uid]
  local mount = dr and (dr.parent or 0)
  return mount ~= nil and mount ~= 0 and axial_departing(dec_uid)[mount] == true
end

-- Which live child node belongs to a departing axial cut: match the live child
-- (vessel-local coords ≈ blueprint coords) to its blueprint uid, then ask the
-- tree whether that uid is on the departing side.
local function child_departs_axial(child, dec_uid)
  local set = axial_departing(dec_uid)
  for uid, q in pairs(bp_by_uid) do
    if math.abs((child.x or 0) - q.x) < 0.06 and math.abs((child.y or 0) - q.y) < 0.06
      and math.abs((child.z or 0) - q.z) < 0.06 then
      return set[uid] == true
    end
  end
  return false -- unmatched: keep it attached rather than eject a mystery part
end

-- Detaching a decoupler's whole subtree also gives away those tanks' share of
-- the pooled fuel and forgets those engines — decided by the departing SET
-- (tree membership), so branch (booster) engines/tanks leave exactly when their
-- subtree does, with no per-part y guessing.
local function drop_departing(dec_uid)
  local set = axial_departing(dec_uid)
  local frac = (fuel_cap > 0) and (fuel / fuel_cap) or 0
  local keep_e, keep_t, cap = {}, {}, 0.0
  for _, e in ipairs(engines) do
    if not set[e.uid] then keep_e[#keep_e + 1] = e end
  end
  for _, t in ipairs(tanks) do
    if not set[t.uid] then
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
-- Tolerances are IMPACT SPEEDS (m/s), like KSP's crash tolerance: the felt
-- Δv is the impulse divided by the vessel's mass, so a 40-tonne ship and a
-- 4-tonne ship judge the same touchdown the same way (absolute impulse
-- thresholds made big ships shred themselves just settling on the pad).
-- Under ~half a part's tolerance a hit is just a landing; past that, damage
-- accumulates (the part smoulders); a full-tolerance blow — or worn-out HP
-- — BREAKS the part: explosion, the part shears off as wreckage, and losing
-- the pod loses the ship. Fuel tanks go up in a real BLAST (see break_part).
-- FRAGILE by design (Ty): a real spacecraft is delicate — a topple, a scrape,
-- a careless bump should HURT, not bounce off. These are low crash speeds, so
-- flying clean matters. Damage starts well under the tolerance (see damage_tick)
-- and a part that's already beaten up breaks far easier.
local TOLERANCE = { crewed = 5, tank = 3.5, engine = 4.5, structural = 6, canvas = 2.5 }
local function part_tolerance(d)
  if (d.legs or 0) == 1 then
    -- Legs exist to hit the ground — but only DEPLOYED ones absorb it.
    return gear_deployed and 9 or 3
  end
  return TOLERANCE[d.kind or "structural"] or 5
end
-- Damage stays DISARMED while clamped/assembling and for a settle window
-- after release — the pad handshake is not a crash.
local damage_arm = 0.0

-- The live child node (and blueprint uid) behind a reported part entity id.
-- The uid is resolved by NEAREST blueprint part (not an exact-pose match) and
-- CACHED: a part that's been knocked out of its authored pose used to miss the
-- old 0.05 threshold and silently drop its impact — so a deflected, grinding
-- or already-wounded part took no further damage. Nearest-match + cache fixes
-- that (the entity id is stable, so one resolve holds for the part's life).
local function uid_of_child(node, eid)
  for _, c in ipairs(node:children()) do
    if c.id == eid then
      local uid = eid_uid[eid]
      if not uid then
        local best, bd = nil, 1e9
        for u, d in pairs(bp_by_uid) do
          local dx, dy, dz = (c.x or 0) - d.x, (c.y or 0) - d.y, (c.z or 0) - d.z
          local dd = dx * dx + dy * dy + dz * dz
          if dd < bd then bd, best = dd, u end
        end
        uid = best
        eid_uid[eid] = uid
      end
      if uid then return uid, c end
    end
  end
  return nil
end

-- Push a part's visible battle damage to its hull shader (the `damage` uniform
-- on hullPanels.flsl: 0 pristine … 1 wrecked — scorch, gouges, dents). Driven
-- straight off the part's remaining HP so the hull visibly deteriorates as you
-- scrape and crash it, exactly what Ty couldn't see before.
local diag_dmg_logged = false
local function set_part_damage(c, hp)
  if c and c.valid then
    local dmg = 1.0 - math.max(0.0, math.min(1.0, hp or 1.0))
    c:setShaderParam("damage", dmg)
    if not diag_dmg_logged and dmg > 0.05 then
      diag_dmg_logged = true
      log(string.format(
        "DMG DIAG: painting damage=%.2f on %s — if the hull shows no scorch/" ..
        "scratches, the material's `damage` uniform isn't reaching the shader.",
        dmg, c.name or "part"))
    end
  end
end

-- The live child node for a blueprint uid (position match) — used to paint
-- damage on parts wounded by a chain reaction they weren't directly hit by.
local function child_of_uid(node, uid)
  local d = bp_by_uid[uid]
  if not d then return nil end
  for _, c in ipairs(node:children()) do
    if math.abs((c.x or 0) - d.x) < 0.06 and math.abs((c.y or 0) - d.y) < 0.06
      and math.abs((c.z or 0) - d.z) < 0.06 then
      return c
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

-- Per-tick peripheral actuation: ease each device toward its on/off state and
-- drive its live child nodes (leg struts fold out/in). VISUAL fold only — the
-- leg's COLLISION uses a stable always-on deployed-footprint box on PartLegs.
-- (An earlier version re-posed the colliders to follow the fold via
-- assembly.syncColliders, but a gear-up craft then had no footprint and slid /
-- bounced — reverted; fold-tracking colliders need a stability floor first.)
local function update_peripherals(node, dt)
  for _, dev in pairs(DEVICES) do
    local target = dev.on and 1.0 or 0.0
    if dev.anim ~= target then
      local step = dev.speed * dt
      dev.anim = (dev.anim < target) and math.min(target, dev.anim + step)
        or math.max(target, dev.anim - step)
    end
    if #dev.parts > 0 and dev.anim_applied ~= dev.anim then
      dev.anim_applied = dev.anim
      for _, uid in ipairs(dev.parts) do
        local ch = dev.nodes[uid]
        if not (ch and ch.valid) then
          ch = nil
          local d = bp_by_uid[uid]
          if d then
            for _, c in ipairs(node:children()) do
              if math.abs((c.x or 0) - d.x) < 0.05 and math.abs((c.z or 0) - d.z) < 0.05
                and math.abs((c.y or 0) - d.y) < 0.45 then
                ch = c
                break
              end
            end
          end
          dev.nodes[uid] = ch
        end
        if ch then
          local pose = dev.pose[uid]
          if not pose then
            pose = { x = ch.x, y = ch.y, z = ch.z, sy = ch.scale_y or 1.0,
                     sx = ch.scale_x or 1.0,
                     pitch = ch.pitch or 0, roll = ch.roll or 0, yaw = ch.yaw or 0 }
            dev.pose[uid] = pose
          end
          dev.apply(dev, ch, pose, dev.anim)
        end
      end
    end
  end
end

-- The brightest body in the system = the star (highest µ). Cached briefly since
-- space.bodies() allocates a fresh table each call.
local function brightest_sun()
  if elec.sun and time - elec.sun_t < 2.0 then return elec.sun end
  local best, bmu = nil, -1
  for _, b in ipairs(space.bodies() or {}) do
    if (b.mu or 0) > bmu then bmu, best = b.mu or 0, b end
  end
  elec.sun, elec.sun_t = best, time
  return best
end

-- Post this live vessel to the shared tracker registry (save key "comms.ships",
-- id → {name,x,y,z,alt,charge,t}) so a future Tracking Station can list every
-- craft that still has a POWERED comms link. Cleared when the link drops.
local function tracker_heartbeat(node, info, alt, live)
  local key = "v" .. tostring(node.id or 0)
  local ships = save.get("comms.ships") or {}
  if live then
    ships[key] = { name = node.name or "Vessel", x = node.x, y = node.y, z = node.z,
                   alt = alt or 0, charge = elec.charge, cap = elec.cap, t = time }
  else
    ships[key] = nil
  end
  save.set("comms.ships", ships)
end

-- ELECTRICAL tick: solar panels (deployed + sunlit) charge the batteries, a
-- deployed comms dish sips EC to hold its telemetry link, and the vessel is
-- posted to / pulled from the tracker as its link comes up or dies. Runs piloted
-- or derelict — a dead-stick probe on panels keeps talking home.
local function power_tick(node, info, dt)
  if elec.cap <= 0 and elec.gen <= 0 and not elec.comms then
    elec.link = false
    return
  end
  -- Generation: deployed panels, scaled by how squarely they face the sun and
  -- whether we're on the lit side of the body we're near.
  local gen = 0.0
  if elec.gen > 0 and DEVICES.solar.on and DEVICES.solar.anim > 0.5 then
    local lit, sun = 1.0, brightest_sun()
    if sun then
      local sx, sy, sz = sun.x - node.x, sun.y - node.y, sun.z - node.z
      local sl = math.sqrt(sx * sx + sy * sy + sz * sz)
      if sl > 1e-6 then sx, sy, sz = sx / sl, sy / sl, sz / sl end
      local dom = space.dominant(node.x, node.y, node.z)
      local db = dom and space.body(dom)
      if db and db.name ~= sun.name then
        local ux, uy, uz = node.x - db.x, node.y - db.y, node.z - db.z
        local ul = math.sqrt(ux * ux + uy * uy + uz * uz)
        if ul > 1e-6 then
          local d = (ux * sx + uy * sy + uz * sz) / ul  -- 1 = noon, <0 = night
          lit = math.max(0.0, math.min(1.0, d * 1.5 + 0.35))
        end
      end
    end
    gen = elec.gen * lit
  end
  -- Draw: a live comms link costs EC (a passive battery-only ship draws nothing).
  local dish_up = elec.comms and DEVICES.comms.on and DEVICES.comms.anim > 0.5
  local draw = dish_up and 1.5 or 0.0
  elec.charge = math.max(0.0, math.min(elec.cap, elec.charge + (gen - draw) * dt))
  -- The link is live only while the dish is out AND the bus isn't flat.
  elec.link = dish_up and (elec.charge > 0.0 or draw == 0.0)
  -- Tracker heartbeat (throttled): altitude above the dominant body.
  if time - elec.ptime > 1.0 then
    elec.ptime = time
    local alt = 0.0
    local dom = space.dominant(node.x, node.y, node.z)
    local db = dom and space.body(dom)
    if db then
      local ax, ay, az = node.x - db.x, node.y - db.y, node.z - db.z
      alt = math.sqrt(ax * ax + ay * ay + az * az) - db.radius
    end
    tracker_heartbeat(node, info, alt, elec.link)
  end
end

-- Canopy visual: an open chute node balloons out (scale up); packed it sits
-- flush. Advances chute_anim whenever deployed and scales the chute nodes.
local chute_nodes = nil
local function update_chutes(node, dt)
  if not chutes_deployed then return end
  chute_anim = math.min(1.0, chute_anim + dt / 0.8)
  if not chute_nodes then
    chute_nodes = {}
    for _, c in ipairs(chutes) do
      for _, ch in ipairs(node:children()) do
        if math.abs((ch.x or 0) - c.x) < 0.05 and math.abs((ch.y or 0) - c.y) < 0.05
          and math.abs((ch.z or 0) - c.z) < 0.05 then
          chute_nodes[#chute_nodes + 1] = { node = ch,
            sx = ch.scale_x or 1.0, sy = ch.scale_y or 1.0, sz = ch.scale_z or 1.0 }
          break
        end
      end
    end
  end
  local grow = 1.0 + chute_anim * 2.2 -- canopy blooms to ~3× when full
  for _, c in ipairs(chute_nodes) do
    if c.node.valid then
      c.node.scale_x = c.sx * grow
      c.node.scale_y = c.sy * (1.0 + chute_anim * 0.6)
      c.node.scale_z = c.sz * grow
    end
  end
end

-- Departing parts take their plumes with them: stop any engine flame on the
-- nodes about to split away (nothing owns them afterwards — un-quenched,
-- a dropped booster's plume burned forever).
local function quench(parts_nodes)
  for _, pn in ipairs(parts_nodes) do
    for _, f in ipairs(pn:children()) do
      if f.name == "Engine Flame" then
        local ps = f:particles()
        if ps and ps:isPlaying() then ps:stop() end
        local light = f:getcomponent("PointLight")
        if light then light.intensity = 0.0 end
      end
    end
  end
end

-- Kick a freshly-split wreck away from the impact with a consistent tumble.
-- TWO fixes for "debris flies off weird": (1) the lift is along GRAVITY-UP
-- (radial on a planetoid — world +Y is sideways when you've landed on a
-- sphere's flank, which threw wreck out horizontally), and (2) the kick is a
-- Δv scaled by the wreck's own mass (impulse = m·Δv) so a 0.15 t decoupler and
-- a 0.8 t engine tumble away at the SAME speed instead of the light bits
-- rocketing off. `dv` is metres/second.
local function debris_kick(junk, hx, hy, hz, dv)
  local ji = assembly.info(junk)
  if not ji then return end
  local cx, cy, cz = ji.com.x, ji.com.y, ji.com.z
  local ox, oy, oz = cx - hx, cy - hy, cz - hz     -- outward from the impact
  local ol = math.sqrt(ox * ox + oy * oy + oz * oz)
  if ol < 1e-3 then ox, oy, oz, ol = 0, 1, 0, 1 end
  ox, oy, oz = ox / ol, oy / ol, oz / ol
  local ux, uy, uz = 0, 1, 0                        -- gravity-up (radial)
  local dom = space.dominant(cx, cy, cz)
  local b = dom and space.body(dom)
  if b then
    local ax, ay, az = cx - b.x, cy - b.y, cz - b.z
    local al = math.sqrt(ax * ax + ay * ay + az * az)
    if al > 1e-3 then ux, uy, uz = ax / al, ay / al, az / al end
  end
  local m = (ji.mass and ji.mass > 0) and ji.mass or 0.2
  assembly.impulseAt(junk, vec3(
    (ox + ux * 0.5) * dv * m,
    (oy + uy * 0.5) * dv * m,
    (oz + uz * 0.5) * dv * m), ji.com)
end

-- Forward-declared: a tank blast can chain into neighbors breaking.
local break_part

-- One-shot diagnostic (once per play session): confirm in the Console that the
-- destruction FX are actually being issued and where, so a "no explosion" report
-- can be pinned to spawn vs render.
local diag_fx_logged = false

break_part = function(node, uid, d, c, hx, hy, hz, depth)
  depth = (depth or 0) + 1
  part_hp[uid] = 0
  -- Ship velocity so the blast's embers/sparks/smoke carry its momentum.
  local _iv = assembly.info(node)
  local evx, evy, evz = 0.0, 0.0, 0.0
  if _iv and _iv.vel then evx, evy, evz = _iv.vel.x, _iv.vel.y, _iv.vel.z end
  spawnEffect("Explosion", hx, hy, hz, evx, evy, evz)
  if not diag_fx_logged then
    diag_fx_logged = true
    log(string.format(
      "FX DIAG: first Explosion spawned at (%.1f, %.1f, %.1f) — if you see no " ..
      "particle here, it's a render issue, not a missing call.", hx, hy, hz))
  end
  -- Fuel tanks AND engines are EXPLOSIVE: they detonate, jolt harder, and can
  -- set off their neighbours (the chain reaction below).
  local is_tank = (d.fuel or 0) > 0
  local is_explosive = is_tank or (d.kind == "engine")
  add_shake(is_explosive and 0.8 or 0.45)
  -- A boom at the break, spatial through the SFX bus; explosive parts go off
  -- deeper and louder. Slight per-part pitch spread so a chain doesn't machine-gun.
  sfx3(is_explosive and SFX.boom or SFX.explode, hx, hy, hz,
    is_explosive and 1.0 or 0.8, 0.92 + (uid % 5) * 0.03)
  drop_part(uid)
  -- Any part failing against the ground leaves a scar; a fuel tank tears a real
  -- crater (bigger with more fuel), an engine a modest one, a strut a scuff.
  local crater_r = is_tank and (2.6 + math.min(4.0, (d.fuel or 0) / 55))
    or (d.kind == "engine" and 2.1 or 1.5)
  make_crater(hx, hy, hz, crater_r)
  log("BOOM: " .. (d.label or d.id or "part") .. " destroyed!")
  -- CHAIN REACTION: every break throws a concussion into the parts STILL
  -- ATTACHED nearby (staged-away parts aren't children — the blast can't reach
  -- bookkeeping ghosts). An EXPLOSIVE part (fuel tank or engine) goes off far
  -- harder — extra fireballs, a shove on the whole vessel, and enough damage to
  -- set off its explosive neighbours, which cook off in turn and ripple through
  -- a tightly-packed stack. Even a structural break sends a lighter shock that
  -- can finish an already-wounded neighbour.
  if depth <= 6 then
    local blast, power
    if is_explosive then
      spawnEffect("Explosion", hx + 0.7, hy + 0.5, hz - 0.4, evx, evy, evz)
      spawnEffect("Explosion", hx - 0.5, hy + 0.9, hz + 0.6, evx, evy, evz)
      local i0 = assembly.info(node)
      if i0 then
        assembly.impulseAt(node, vec3(0, i0.mass * 1.5, 0), vec3(hx, hy, hz))
      end
      blast, power = crater_r, 0.9
    else
      blast, power = crater_r * 0.8, 0.4
    end
    for u2, d2 in pairs(bp_by_uid) do
      local hp2 = part_hp[u2]
      if u2 ~= uid and hp2 and hp2 > 0 then
        local dx2, dy2, dz2 = d2.x - d.x, d2.y - d.y, d2.z - d.z
        local dist = math.sqrt(dx2 * dx2 + dy2 * dy2 + dz2 * dz2)
        if dist < blast then
          local c2 = nil -- the live child for u2, matched by position
          for _, ch in ipairs(node:children()) do
            if math.abs((ch.x or 0) - d2.x) < 0.05 and math.abs((ch.y or 0) - d2.y) < 0.05
              and math.abs((ch.z or 0) - d2.z) < 0.05 then
              c2 = ch
              break
            end
          end
          if c2 then
            part_hp[u2] = hp2 - (1.0 - dist / blast) * power
            if part_hp[u2] <= 0 then
              break_part(node, u2, d2, c2, hx, hy, hz, depth)
            end
          end
        end
      end
    end
  end
  if d.kind == "crewed" then
    -- The POD is the ship. Its pilot is thrown clear of the wreck.
    destroyed = true
    silence_loops()
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
    quench({ c })
    assembly.split(node, { c }, function(junk)
      debris_kick(junk, hx, hy, hz, 5.0)
    end)
  end
end

-- A CATASTROPHIC hit doesn't just crush the contact parts — the airframe FAILS
-- and the ship comes apart. Every surviving part shears into its own tumbling
-- wreck kicked away from the impact, over a big crater and a low boom. Latched
-- by `destroyed` so it fires exactly once.
local function shatter(node, hx, hy, hz, spd, info)
  if destroyed then return end
  destroyed = true
  log("CATASTROPHIC BREAKUP")
  add_shake(1.4) -- the whole airframe failing is a hard jolt
  local evx, evy, evz = 0.0, 0.0, 0.0
  if info and info.vel then evx, evy, evz = info.vel.x, info.vel.y, info.vel.z end
  spawnEffect("Explosion", hx, hy, hz, evx, evy, evz)
  spawnEffect("Explosion", hx + 0.8, hy + 0.6, hz - 0.5, evx, evy, evz)
  spawnEffect("Explosion", hx - 0.6, hy + 0.3, hz + 0.7, evx, evy, evz)
  sfx3(SFX.boom, hx, hy, hz, 1.0, 0.85)
  sfx3(SFX.explode, hx, hy, hz, 0.9, 1.05)
  spawnEffect("Explosion", hx + 0.2, hy + 1.4, hz + 0.3, evx, evy, evz) -- a secondary up high
  make_crater(hx, hy, hz, 4.8)
  silence_loops()
  -- Throw the pilot clear of the wreck.
  if piloting then
    piloting = false
    if astronaut then
      astronaut.visible = true
      astronaut.x, astronaut.y, astronaut.z = hx, hy + 2.0, hz
      if info then
        astronaut.vx = info.vel.x
        astronaut.vy = info.vel.y + 3.0
        astronaut.vz = info.vel.z
      end
    end
    set_hud(nil)
    set_navball(false)
    set_stage_list(nil)
    set_prompt("")
  end
  -- Shear every part loose (leave the last one so the compound stays valid).
  local live = {}
  for _, ch in ipairs(node:children()) do live[#live + 1] = ch end
  local kick = math.min(11.0, spd * 0.22)
  for i = 1, #live - 1 do
    local ch = live[i]
    if ch and ch.valid then
      quench({ ch })
      assembly.split(node, { ch }, function(junk)
        debris_kick(junk, hx, hy, hz, kick)
      end)
    end
  end
  for uid in pairs(part_hp) do part_hp[uid] = 0 end
end

-- TOTAL closing speed (m/s) above which a hit fails the whole airframe, not just
-- the parts it touched — a genuine lithobrake, not a hard landing. Judged on the
-- honest energy metric now (speedAbs), so a fast ram at ANY angle triggers it.
local SHATTER_SPEED = 13.0
-- How fast a surviving-but-overloaded part loses HP, per second, at full
-- overload (a hit just short of its crash tolerance). A brief bump chips; a
-- sustained grind tears through in a fraction of a second. Fragile by design.
local DAMAGE_RATE = 3.0
-- One-shot latch for the stale-engine warning (see damage_tick).
local engine_version_warned = false
-- Throttled crash diagnostics to the Console (contact count / worst speed / terrain
-- proximity). Leave on while tuning crashes; the log is rate-limited to twice a second.
local DAMAGE_DEBUG = true
local dbg_t = 0

local function damage_tick(node, info, dt)
  -- Disarmed while clamped or still assembling; release starts a settle
  -- window — the pad handshake must never read as a crash (big ships were
  -- shredding themselves the moment they spawned).
  if info.anchored or (launch_mode and not released) then
    damage_arm = time
    return
  end
  if time - damage_arm < 2.5 then return end
  -- SEVERITY is the honest ENERGY of each contact: `speedAbs`, the TOTAL closing
  -- speed. The old model judged by `speed` — the NORMAL component only — which
  -- collapses on a glancing hit or a hit against a CURVED planet (a 30 m/s ram
  -- 65° off-normal read ~12 m/s and bounced off). Total speed captures a ram, a
  -- topple and a belly-slide with ONE number, so it drives every crash effect:
  -- no more dead-zone between a "normal hit" and a "scrape". Pre-scan every live
  -- part's hardest hit (and the worst overall) BEFORE applying anything, so a
  -- catastrophic blow fails the whole airframe even if it landed on the pod.
  local hits = {}
  local worst, wx, wy, wz = 0.0, nil, nil, nil
  local raw = assembly.impacts(node)
  for _, h in ipairs(raw) do
    -- STALE-ENGINE GUARD: speedAbs is a NEW engine field. If it's missing, the
    -- editor binary predates this session's physics work — damage falls back to
    -- the old normal-speed metric (bounces, hard to break) and the anim/particle
    -- fixes aren't in either. Shout ONCE so it's obvious a rebuild is needed.
    if h.speedAbs == nil and not engine_version_warned then
      engine_version_warned = true
      log("[!] ENGINE OUT OF DATE — impacts have no `speedAbs`. Rebuild/reinstall " ..
        "the editor: the damage energy-metric, particle and animation fixes are " ..
        "in the Rust binary, not the Lua. Run `cargo run -p floptle-editor` (or " ..
        "reinstall the freshly-built version).")
    end
    local uid, c = uid_of_child(node, h.part)
    local d = uid and bp_by_uid[uid]
    local hp = uid and part_hp[uid]
    if d and hp and hp > 0 then
      local nrm = h.speed or 0
      local energy = math.max(h.speedAbs or 0, nrm)             -- total closing speed
      local slide = math.sqrt(math.max(0.0, energy * energy - nrm * nrm))
      hits[#hits + 1] = { uid = uid, c = c, d = d, hp = hp,
                          e = energy, slide = slide, x = h.x, y = h.y, z = h.z }
      if energy > worst then worst, wx, wy, wz = energy, h.x, h.y, h.z end
    end
  end
  -- DIAGNOSTIC: when contacts arrive, report (throttled) what the crash model sees —
  -- contact count, the worst closing speed vs the shatter threshold, and whether the
  -- terrain query finds ground at the impact. This turns "it just slides / no crater"
  -- into hard data: no line at all = the compound reports ZERO terrain contacts (LOD/
  -- residency/anchor), a line with worst≈0 = grazing-only, a line with sd=nil = the
  -- crater is gated out because no resident terrain is under the hit.
  if DAMAGE_DEBUG and #raw > 0 and time - dbg_t > 0.5 then
    dbg_t = time
    local sd = wx and terrain.query(wx, wy, wz)
    log(string.format(
      "CRASH DIAG: %d contact(s), worst=%.1f m/s (shatter %.0f), terrain sd=%s",
      #raw, worst, SHATTER_SPEED, sd and string.format("%.2f", sd) or "nil (no ground here)"))
  end
  -- A genuinely violent strike fails the WHOLE airframe — the ship bursts apart.
  if not destroyed and worst >= SHATTER_SPEED and wx then
    shatter(node, wx, wy, wz, worst, info)
    return
  end
  local spark = time - scrape_t > 0.10
  local dug = false
  -- Ship world velocity, handed to crash effects so sparks/dust/debris ride its
  -- momentum instead of being stranded in space (inherit_velocity on the effects).
  local vv = info.vel
  local vx, vy, vz = vv.x, vv.y, vv.z
  for _, h in ipairs(hits) do
    local live = part_hp[h.uid] or 0   -- a chain reaction may have claimed it first
    if live > 0 then
      -- Wounded parts are weaker: tolerance scales with remaining HP, so repeated
      -- hits COMPOUND. A hit that MEETS the (HP-scaled) crash tolerance crushes
      -- the part on contact; between the ~35%-of-tolerance floor and that limit
      -- it WEARS at a dt-scaled rate set by how far past the floor it landed —
      -- a brief bump scuffs, a sustained grind tears through in a fraction of a
      -- second. Legs get the same treatment but a far higher tolerance (they're
      -- built to hit the ground — when deployed).
      local tol = part_tolerance(h.d) * (0.45 + 0.55 * live)
      local floor = tol * 0.35
      if h.e >= tol then
        set_part_damage(h.c, 0)
        part_hp[h.uid] = 0
        break_part(node, h.uid, h.d, h.c, h.x, h.y, h.z)
      elseif h.e >= floor then
        local over = (h.e - floor) / math.max(0.5, tol - floor)   -- 0 … 1
        part_hp[h.uid] = live - over * DAMAGE_RATE * dt
        set_part_damage(h.c, part_hp[h.uid])
        if part_hp[h.uid] <= 0 then
          break_part(node, h.uid, h.d, h.c, h.x, h.y, h.z)
        elseif spark then
          -- A part grinding on the surface throws hot sparks and kicks up gritty
          -- dust (not soft smoke) — both inherit the ship's velocity so they streak
          -- along the scrape instead of puffing straight up.
          spawnEffect("Sparks", h.x, h.y, h.z, vx, vy, vz)
          spawnEffect("ScrapeDust", h.x, h.y, h.z, vx, vy, vz)
          add_shake(math.min(0.4, 0.1 + over * 0.3))
          sfx3(SFX.stage, h.x, h.y, h.z, math.min(0.6, 0.25 + over * 0.4), 1.2)
        end
        -- TERRAIN DEFORMATION: a hull biting the ground GOUGES it — you can't
        -- grind or ram the surface without carving a scar (and that scar is why
        -- the ship keeps taking damage as it drags). Rate-limited to one dig per
        -- beat so a drag cuts a trench, not a canyon; only when the contact is
        -- genuinely on terrain (legs are exempt — they're meant to touch down).
        if spark and not dug and (h.d.legs or 0) ~= 1 then
          local r = 0.6 + math.min(2.0, h.e * 0.12)
          local sd = terrain.query(h.x, h.y, h.z)
          if sd and sd < r then
            terrain.dig(h.x, h.y, h.z, r, 0.6)
            -- SCUFF the surface where the hull bit in: a dark scratch mark painted
            -- over a wider radius than the gouge (color-only — no remesh cost), so
            -- the scar reads even where the dig is shallow.
            terrain.paint(h.x, h.y, h.z, r * 1.4, 0.12, 0.1, 0.09, 0.55)
            dug = true
          end
        end
      end
      if destroyed then break end
    end
  end
  if spark then scrape_t = time end
  if destroyed then return end
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
          -- Keep the visible damage in sync (a part wounded by a CHAIN reaction
          -- it wasn't directly hit by still shows its scars).
          set_part_damage(child_of_uid(node, uid), hp)
          spawnEffect("Smoke",
            bx + rx.x * d.x + up.x * d.y + rz.x * d.z,
            by + rx.y * d.x + up.y * d.y + rz.y * d.z,
            bz + rx.z * d.x + up.z * d.y + rz.z * d.z,
            info.vel.x, info.vel.y, info.vel.z)
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
  if info then damage_tick(node, info, dt) end
  -- (Electrical power_tick runs in lateUpdate — fixedUpdate is at Lua's upvalue
  -- ceiling, so the power system lives in the post-writeback pass instead.)
  if destroyed then
    set_prompt("")
    -- A lost vessel drops off the tracker (inlined so fixedUpdate doesn't spend
    -- another upvalue on tracker_heartbeat — it's near Lua's 60-upvalue ceiling).
    if elec.link then
      elec.link = false
      local ships = save.get("comms.ships") or {}
      ships["v" .. tostring(node.id or 0)] = nil
      save.set("comms.ships", ships)
    end
    -- The breakup boom still shakes the (now on-foot) view, then settles.
    shake = shake * math.max(0.0, 1.0 - dt * 5.0)
    publish_shake(0.0)
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
  -- waits its turn. No axial cuts left = every remaining engine fires.
  local cut = math.huge
  for _, ev in ipairs(events) do
    if ev.kind == "axial" and ev.y < cut then cut = ev.y end
  end

  -- ---- board / exit -------------------------------------------------------
  if input.pressed("f") and astronaut then
    if piloting then
      piloting = false
      throttle = 0.0
      set_flames(node, 0)
      silence_loops() -- engine/reentry/bed all stand down when you step out
      throttle_prev = 0.0
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
    -- Handed off (EVA, lost, staged): rejoin the LOD if we were kept live.
    if kept_live then
      assembly.keepLive(node, false)
      kept_live = false
    end
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

  -- ---- the map is a fly-on-instruments view for a built vessel -----------
  -- KSP lets you FLY from the map (throttle up watching your trajectory bend,
  -- steer, stage), so a piloted vessel keeps FULL control while the map is
  -- open — throttle, stick, SAS, staging, gear all live. (Maneuver-node
  -- PLANNING with WASD is the scout ship's job; ship_controller only takes the
  -- keyboard for planning when the SCOUT is the subject, never a vessel — so
  -- there's no key clash.) `map_open` now only suppresses our HUD text, which
  -- the map repaints on the shared node.
  local sc_map = findScript("ship_controller")
  local map_open = (sc_map and sc_map.map_view) or false
  -- Flying from the map pulls the camera far back; keep this vessel exempt from
  -- distant-craft LOD while it's open so throttle/steering stay live and our
  -- orbital velocity keeps feeding the trajectory. Released when the map closes.
  if map_open ~= kept_live then
    assembly.keepLive(node, map_open)
    kept_live = map_open
  end

  -- ---- time warp (compounds coast on their own Kepler rails now) ---------
  local warp = space.warp()
  if input.pressed(".") or input.pressed(",") then
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
  -- Hands on the stick cancel warp — the rails own the ship up there. (This
  -- holds in the map too now: the map is a live flying view, so any control
  -- input there should still drop warp, same as out of it.)
  if warp > 1.001 then
    local touched = input.key("shift") or input.key("ctrl") or input.key("z")
      or input.key("w") or input.key("a") or input.key("s") or input.key("d")
      or input.key("q") or input.key("e")
    if touched then
      space.warp(1)
      warp = 1
    end
  end

  -- ---- throttle + pooled fuel (live in the map too) ----------------------
  if input.key("shift") then throttle = throttle + params.throttle_rate * dt end
  if input.key("ctrl") then throttle = throttle - params.throttle_rate * dt end
  if input.key("x") then throttle = 0.0 end
  if input.key("z") then throttle = 1.0 end
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

  -- ---- engine audio: ignition crack + a loop that rides the throttle ------
  local burning = throttle > 0.02 and not info.anchored and fuel > 0 and total_thrust > 0
  if burning and throttle_prev <= 0.02 then
    -- The ignition crack fires at the base of the stack.
    sfx3(SFX.ignite, bx, by, bz, 0.9, 1.0)
  end
  update_engine_audio(node, burning, throttle)
  throttle_prev = burning and throttle or 0.0

  -- ---- ground FX: touchdown dust + a rolling exhaust cloud under thrust ----
  -- The ground contact point is a touch below the stack base.
  local gsd = terrain.query(bx, by, bz)
  local near_ground = info.grounded or (gsd ~= nil and gsd < 4.0)
  if info.grounded and not was_grounded then
    -- Only kick up dust for a real touchdown, not a creep onto the pad.
    local vv = info.vel
    local sp = math.sqrt(vv.x * vv.x + vv.y * vv.y + vv.z * vv.z)
    if sp > 1.5 then
      spawnEffect("TouchDust", bx, by, bz)
      sfx3(SFX.clamp, bx, by, bz, math.min(0.9, 0.3 + sp * 0.05), 0.8)
      add_shake(math.min(0.5, sp * 0.03)) -- a firm touchdown thuds the view
    end
  end
  was_grounded = info.grounded or false
  if burning and near_ground and time - launch_dust_t > 0.16 then
    launch_dust_t = time
    spawnEffect("LaunchDust", bx, by, bz)
  end

  -- ---- reentry / atmospheric heating -------------------------------------
  -- Below the atmo band (alt < 35% of the body's radius) the air bites: WIND
  -- roars (proportional to density × speed), a mild aero DRAG bleeds you when
  -- you coast in fast (so slowing down actually matters — Ty's ask), and past
  -- ~35 m/s the windward parts turn speed into HEAT (flux ~ density^1.5 · v³)
  -- — fire licks off them and a part that cooks through breaks up in flight.
  heating = 0.0
  wind_intensity = 0.0
  do
    -- space.dominant can return nil while the galaxy is still streaming in
    -- (right after a launch, before the dominant body is resolved); guard it
    -- or space.body(nil) throws every frame and aborts the rest of the update.
    local dom2 = space.dominant(node.x, node.y, node.z)
    local db2 = dom2 and space.body(dom2)
    if db2 and not info.anchored and not info.grounded then
      local dxh, dyh, dzh = node.x - db2.x, node.y - db2.y, node.z - db2.z
      local alt = math.sqrt(dxh * dxh + dyh * dyh + dzh * dzh) - db2.radius
      local band = db2.radius * 0.35
      local v = info.vel
      local spd = math.sqrt(v.x * v.x + v.y * v.y + v.z * v.z)
      -- Wind + drag live across the WHOLE band (not just when hot).
      if alt < band and alt > 0 then
        local dens = 1.0 - alt / band
        wind_intensity = dens * math.min(1.4, math.max(0.0, (spd - 8.0) / 80.0))
        -- Aero drag opposes velocity through the CoM (∝ density · v²). Gated to
        -- fast UNPOWERED flight: coasting/reentry is where "slow down" bites,
        -- and it leaves powered ascent (and the tuned smoke force counts) alone.
        if spd > 45.0 and throttle < 0.02 then
          local dk = 0.0045
          assembly.force(node, vec3(
            -v.x * spd * dens * dk, -v.y * spd * dens * dk, -v.z * spd * dens * dk))
        end
        -- VISUAL reentry glow, DECOUPLED from damage: plasma licks off the windward
        -- side from ~45 m/s (scaled by density × speed), so a normal descent looks
        -- hot even though the part-cooking DAMAGE pass below only bites past 90 m/s.
        -- (Previously the Heat effect only ever spawned inside the >90 damage gate, so
        -- ordinary reentries showed nothing.) Streams off the nose, riding ship velocity.
        local glow = math.max(0.0, (spd - 45.0) / 130.0) * dens
        if glow > 0.04 and time - heat_fx_t > (0.30 - math.min(0.18, glow * 0.2)) then
          heat_fx_t = time
          local gs = math.max(spd, 1e-3)
          local gvx, gvy, gvz = v.x / gs, v.y / gs, v.z / gs
          local grx, gup, grz = basis(node)
          local gbx, gby, gbz = base_of(node, info)
          spawnEffect("Heat",
            gbx + gvx * 1.2 + gup.x * 0.8,
            gby + gvy * 1.2 + gup.y * 0.8,
            gbz + gvz * 1.2 + gup.z * 0.8,
            v.x, v.y, v.z)
        end
      end
      if alt < band and alt > 0 and spd > 90.0 then
        -- Heating only bites at genuinely reckless speed now. The old curve
        -- (onset 35 m/s, (spd/40)³·0.06) cooked a normal ascent/descent — a
        -- ~146 m/s craft burned up in a second, which made the ship untestable.
        -- Onset 90 m/s, a gentler cube and coefficient: you can fly fast, but a
        -- screaming-fast reentry (250+ m/s) still lights you up over a few s.
        local density = (1.0 - alt / band)
        heating = density ^ 1.5 * (spd / 130.0) ^ 3 * 0.03 -- dmg/sec at the nose
        local ivx, ivy, ivz = v.x / spd, v.y / spd, v.z / spd
        local rx2, up2, rz2 = basis(node)
        -- Rank parts by how far forward they sit along the velocity.
        local best_proj = -math.huge
        for uid2, d2 in pairs(bp_by_uid) do
          if part_hp[uid2] and part_hp[uid2] > 0 then
            local wx = rx2.x * d2.x + up2.x * d2.y + rz2.x * d2.z
            local wy = rx2.y * d2.x + up2.y * d2.y + rz2.y * d2.z
            local wz = rx2.z * d2.x + up2.z * d2.y + rz2.z * d2.z
            local proj = wx * ivx + wy * ivy + wz * ivz
            if proj > best_proj then best_proj = proj end
          end
        end
        for uid2, d2 in pairs(bp_by_uid) do
          local hp2 = part_hp[uid2]
          if hp2 and hp2 > 0 then
            local wx = rx2.x * d2.x + up2.x * d2.y + rz2.x * d2.z
            local wy = rx2.y * d2.x + up2.y * d2.y + rz2.y * d2.z
            local wz = rx2.z * d2.x + up2.z * d2.y + rz2.z * d2.z
            local proj = wx * ivx + wy * ivy + wz * ivz
            -- Windward third takes the flux; leeward parts ride in the wake.
            if proj > best_proj - 1.0 then
              part_hp[uid2] = hp2 - heating * dt
              if part_hp[uid2] <= 0 then
                local px2, py2, pz2 = bx + wx, by + wy, bz + wz
                local c2 = nil
                for _, ch in ipairs(node:children()) do
                  if math.abs((ch.x or 0) - d2.x) < 0.05 and math.abs((ch.y or 0) - d2.y) < 0.05
                    and math.abs((ch.z or 0) - d2.z) < 0.05 then
                    c2 = ch
                    break
                  end
                end
                break_part(node, uid2, d2, c2, px2, py2, pz2)
                log("burned up on reentry: " .. (d2.label or d2.id or "part"))
              end
            end
          end
        end
        -- (The plasma VFX is now spawned by the decoupled glow block above, which
        -- covers this >90 regime too — no separate spawn here.)
      end

      -- ---- PARACHUTE DRAG: the reason the atmosphere is here -------------
      -- An open canopy in thick air drags HARD against velocity, opposing
      -- your fall — a soft touchdown instead of a lithobraking. Drag scales
      -- with air density and v² (real aero), pooled across every open chute,
      -- applied through the CoM so it just decelerates (no tumble).
      if chutes_deployed and #chutes > 0 and alt < band and alt > 0 then
        -- Drag ∝ air density · v² (real aero), pooled across every open chute
        -- and scaled by how full the canopy is (chute_anim, driven by
        -- update_chutes). Through the CoM so it decelerates without tumbling.
        local density = 1.0 - alt / band
        local drag_k = 0.9 * #chutes * chute_anim
        local fx3 = -v.x * spd * density * drag_k
        local fy3 = -v.y * spd * density * drag_k
        local fz3 = -v.z * spd * density * drag_k
        if not info.anchored then
          assembly.force(node, vec3(fx3, fy3, fz3))
        end
      end
    else
      chute_anim = 0.0
    end
  end
  -- The roar rides the wind (density × speed) and climbs with reentry heat.
  do
    local v = info.vel
    local spd = math.sqrt(v.x * v.x + v.y * v.y + v.z * v.z)
    update_wind_audio(node, wind_intensity, heating, spd)
  end

  -- ---- screen shake: decay the transient, add the continuous, publish -----
  shake = shake * math.max(0.0, 1.0 - dt * 5.0)
  local base = 0.0
  if piloting then
    base = math.min(0.55, wind_intensity * 0.4)            -- atmospheric buffet
    if burning and near_ground then base = base + throttle * 0.14 end -- liftoff rumble
    if heating > 0.05 then base = base + math.min(0.35, heating * 1.5) end
  end
  publish_shake(base)

  -- ---- attitude: rate-commanded, the KSP feel ----------------------------
  if input.pressed("t") then
    sas_mode = (sas_mode ~= "off") and "off" or sas_last
  end
  for k, m in pairs(SAS_KEYS) do
    if input.pressed(k) then setSAS(m) end
  end
  -- Peripherals: each device kind has its key (G = landing gear).
  for _, dev in pairs(DEVICES) do
    if #dev.parts > 0 and input.pressed(dev.key) then
      dev.on = not dev.on
      local bx2, by2, bz2 = base_of(node, info)
      sfx3(SFX.gear, bx2, by2, bz2, 0.7, dev.on and 1.0 or 0.9)
      log(dev.label .. (dev.on and " deployed" or " retracted"))
    end
  end
  gear_deployed = DEVICES.gear.on
  update_peripherals(node, dt)
  update_chutes(node, dt)
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
    local ddom = space.dominant(node.x, node.y, node.z)
    local db = ddom and space.body(ddom)
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

  -- ---- SPACE: clamps first, then boosters, then axial stages (live in map) -
  if input.pressed("space") then
    if info.anchored then
      if assembly_ready(info) then
        released = true
        assembly.setAnchored(node, false)
        sfx3(SFX.clamp, bx, by, bz, 0.9, 1.0) -- the clamps let go with a clank
        log("launch clamps released")
      else
        log(string.format("clamps hold: %d / %d parts assembled", #info.parts, part_total))
      end
    elseif launch_mode and not released then
      -- SELF-HEAL: a mid-launch sim rebuild (terrain streaming on loaded saves)
      -- can clear the compound's anchored flag before we ever released. Without
      -- this, SPACE would fall through to staging and `released` never latches —
      -- leaving damage_tick disarmed forever (the "ship bounces, never crashes
      -- on a loaded save" bug). The Rust rebuild_sim fix keeps us anchored so we
      -- normally take the branch above; this catches any residual un-anchor.
      released = true
      assembly.setAnchored(node, false)
      sfx3(SFX.clamp, bx, by, bz, 0.9, 1.0)
      log("launch clamps released")
    elseif #events > 0 and events[1].kind == "chute" then
      -- PARACHUTES: pop them open. They start dragging (see the drag pass
      -- below) as soon as they're in thick enough air.
      table.remove(events, 1)
      chutes_deployed = true
      sfx3(SFX.chute, bx, by, bz, 0.8, 1.0) -- the canopy cracks open
      log("parachutes deployed")
    elseif #events > 0 and events[1].kind == "ring" then
      -- A BOOSTER RING: every branch in the ring kicks away laterally at
      -- once (symmetric pairs leave together, so the ship stays balanced).
      local ev = table.remove(events, 1)
      for _, g in ipairs(ev.branches) do
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
          quench(parts_nodes)
          sfx3(SFX.boosters, bx, by, bz, 0.85, 1.0) -- the separation charge fires
          assembly.split(node, parts_nodes, function(stage)
            local si = assembly.info(stage)
            if si then
              -- Mass-scaled Δv: a FLAT impulse gave a heavy booster almost no
              -- push (Δv = impulse/mass), so close-mounted rings clung to the
              -- hull and looked stuck. Multiply by mass for a consistent ~4.5
              -- m/s lateral kick whatever the stage weighs. And nudge the origin
              -- a hair OUTWARD first to clear the contact the solver would
              -- otherwise immediately re-close — the actual "won't detach" cause.
              local dv, m = 4.5, math.max(0.1, si.mass)
              if si.origin then
                assembly.teleport(stage, vec3(
                  si.origin.x + kx * 0.35, si.origin.y + ky * 0.35, si.origin.z + kz * 0.35))
              end
              assembly.impulseAt(stage, vec3(kx * dv * m, ky * dv * m, kz * dv * m), si.com)
              spawnEffect("SepPuff", si.com.x, si.com.y, si.com.z)
            end
            log("boosters away: " .. n_away .. " parts")
          end)
        end
      end
    elseif #events > 0 then
      local dec = table.remove(events, 1) -- an AXIAL cut
      -- The dropped set is TOPOLOGICAL: the decoupler's whole subtree by the
      -- attachment tree (pod side stays), including any radial branches bolted
      -- to the discarded stack — a separate booster stage on the survivor stays.
      local dep = axial_departing(dec.uid)
      local parts_nodes = {}
      for _, child in ipairs(node:children()) do
        if child_departs_axial(child, dec.uid) then parts_nodes[#parts_nodes + 1] = child end
      end
      if #parts_nodes > 0 then
        drop_departing(dec.uid)
        -- Events ride away with the parts they act on: an axial cut or a ring
        -- whose decoupler sat in the discarded subtree leaves; events on the
        -- surviving pod side stay. Purely by tree membership now.
        local keep = {}
        for _, e2 in ipairs(events) do
          if e2.kind == "axial" then
            if not dep[e2.uid] then keep[#keep + 1] = e2 end
          elseif e2.kind == "chute" then
            keep[#keep + 1] = e2 -- chutes ride the surviving stack
          else
            local bs = {}
            for _, g in ipairs(e2.branches) do
              if not branch_departs(g.uid, dec.uid) then
                bs[#bs + 1] = g
              else
                drop_branch(g.uid) -- its mount left with the stack
              end
            end
            if #bs > 0 then
              e2.branches = bs
              keep[#keep + 1] = e2
            end
          end
        end
        events = keep
        local n_away = #parts_nodes
        quench(parts_nodes)
        sfx3(SFX.stage, bx, by, bz, 0.9, 1.0) -- the decoupler bangs
        assembly.split(node, parts_nodes, function(stage)
          local si = assembly.info(stage)
          if si then
            -- Mass-scaled Δv (see the ring case) + a small downward nudge so a
            -- heavy spent stage actually falls clear of the surviving stack
            -- instead of riding along in contact.
            local dv, m = 3.5, math.max(0.1, si.mass)
            if si.origin then
              assembly.teleport(stage, vec3(
                si.origin.x - nx * 0.3, si.origin.y - ny * 0.3, si.origin.z - nz * 0.3))
            end
            assembly.impulseAt(stage, vec3(nx * -dv * m, ny * -dv * m, nz * -dv * m), si.com)
            spawnEffect("SepPuff", si.com.x, si.com.y, si.com.z)
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
    -- The map owns the screen while it's open: its info panel IS the shared
    -- "Ship HUD Text" node, repainted by the scout's map on its own 10 Hz
    -- clock. We must NOT also write that node — a `set_hud(nil)` here every
    -- 0.1 s fought the scout's `set_hud(text)` every 0.1 s, and the two
    -- independent clocks flip-flopped the node's visibility (Ty's map-HUD
    -- flicker). Leave the HUD to the scout; only hide OUR stage list.
    if map_open then
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
      (info.anchored and "   CLAMPED" or (info.grounded and "   LANDED" or ""))
        .. ((#DEVICES.gear.parts > 0 and not gear_deployed) and "   GEAR UP" or ""),
      #events > 0 and string.format("   STAGES %d", #events) or "",
      warp > 1.001 and string.format("   [WARP %d×]", warp) or "")
    if fuel_cap > 0 then
      local fbars = math.floor(fuel / fuel_cap * 10 + 0.5)
      local ftag = ""
      if refueling then ftag = "  REFUELING"
      elseif fuel <= 0.0 then ftag = "  [TANK EMPTY]" end
      lines[2] = string.format("FUEL [%s%s] %3d%%%s",
        string.rep("|", fbars), string.rep("·", 10 - fbars), fuel / fuel_cap * 100, ftag)
    end
    -- POWER: only shown on an electrically-equipped craft. Bar = battery state;
    -- tags call out solar charging, a live comms link, or a flat bus.
    if elec.cap > 0 or elec.comms or elec.gen > 0 then
      local frac = (elec.cap > 0) and (elec.charge / elec.cap) or 0
      local ebars = math.floor(frac * 10 + 0.5)
      local etag = ""
      if elec.gen > 0 and DEVICES.solar.on and DEVICES.solar.anim > 0.5 then etag = "  [SOLAR]" end
      if elec.link then etag = etag .. "  [LINK]"
      elseif elec.comms and not DEVICES.comms.on then etag = etag .. "  (dish stowed — U)" end
      if elec.cap > 0 and elec.charge <= 0.0 then etag = etag .. "  [BATTERY FLAT]" end
      lines[#lines + 1] = string.format("PWR  [%s%s] %3d%%%s",
        string.rep("|", ebars), string.rep("·", 10 - ebars), frac * 100, etag)
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
      if spd >= vesc then tag = "  [ESCAPING]"
      elseif spd >= vesc * 0.93 then tag = "  near escape" end
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
        lines[#lines + 1] = "> SPACE — release launch clamps"
      else
        lines[#lines + 1] = string.format("assembling…  %d / %d parts", #info.parts, part_total)
      end
    end
    if heating > 0.02 then
      lines[#lines + 1] = string.format("[HEAT] REENTRY %s— slow down or climb",
        heating > 0.12 and "(SEVERE) " or "")
    end
    if chutes_deployed then
      lines[#lines + 1] = string.format("[CHUTES OPEN]  %d%%", chute_anim * 100)
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
      lines[#lines + 1] = string.format("[DAMAGE]  %s %d%%",
        (d and (d.label or d.id)) or "part", worst_hp * 100)
    end
    lines[#lines + 1] =
      "[SPACE] stage · [T] SAS · [G] gear · [.,] warp · [M] map · [F] exit"
    set_hud(table.concat(lines, "\n"))

    -- The stage list (right edge): SEPARATION EVENTS in the builder's
    -- firing order, drawn bottom-up — the NEXT one sits at the bottom
    -- marked ▶ (SPACE fires it); rows fall off as you stage. A header
    -- shows what's burning right now.
    local n_act, th_act = 0, 0
    for _, e in ipairs(engines) do
      if engine_active(e, cut) then
        n_act = n_act + 1
        th_act = th_act + e.thrust
      end
    end
    local sl = { "STAGES — SPACE fires next",
      string.format("burning: %d engine%s  %d kN", n_act, n_act == 1 and "" or "s", th_act), "" }
    for i = #events, 1, -1 do
      local ev = events[i]
      local tag = (i == 1) and "  «" or ""
      if ev.kind == "ring" then
        local n_eng, th = 0, 0
        for _, e in ipairs(engines) do
          if e.branch then
            n_eng = n_eng + 1
            th = th + e.thrust
          end
        end
        sl[#sl + 1] = string.format("BOOSTER RING ×%d   %d eng  %d kN%s",
          #ev.branches, n_eng, th, tag)
      elseif ev.kind == "chute" then
        sl[#sl + 1] = string.format("CHUTES ×%d%s", #ev.uids, tag)
      else
        sl[#sl + 1] = string.format("STAGE SEP   (below y %.1f)%s", ev.y, tag)
      end
    end
    set_stage_list(table.concat(sl, "\n"))
  end
end

-- Camera-pass drawing: the hatch ring must sit on the ship THE FRAME RENDERS
-- (lateUpdate reads post-writeback poses) — drawn from fixedUpdate it floats
-- a rails-carry beside the hull on any orbiting world.
function lateUpdate(node, dt)
  -- Electrical system runs here (post-writeback): solar charge, comms draw and
  -- the tracker heartbeat. It lives in lateUpdate, not fixedUpdate, purely
  -- because fixedUpdate is at Lua's 60-upvalue ceiling. Runs piloted OR derelict.
  if not destroyed then
    local einfo = assembly.info(node)
    if einfo then power_tick(node, einfo, dt) end
  end
  if piloting or not astronaut or not astronaut.visible then return end
  local info = assembly.info(node)
  if board_dist(node, astronaut, info) > params.board_range + 1.4 then return end
  local rx, up, rz = basis(node)
  local px = node.x + rx.x * pod.x + up.x * pod.y + rz.x * pod.z
  local py = node.y + rx.y * pod.x + up.y * pod.y + rz.y * pod.z
  local pz = node.z + rx.z * pod.x + up.z * pod.y + rz.z * pod.z
  draw.ring(px, py, pz, up.x, up.y, up.z, 0.75, 0.3, 0.95, 1.0, 0.9)
end
