-- TOOL BELT — what the astronaut is holding, and what it does to the world.
--
--   1  MINING LASER    hold E: cuts rock and streams the material into your pack
--   2  HARVEST CUTTER  hold E: takes down the plant you're facing
--   3  TERRAIN SPADE   hold E to lower ground, Q to raise it — shaping only,
--                      it yields nothing (that's the laser's job now)
--
--   I  inventory        (the panel; it also handles ship/warehouse transfers)
--
-- The laser and the spade both aim along the CAMERA's view line (planet_camera
-- publishes it), so what the crosshair is over is what you hit — casting from
-- the character puts a third-person parallax offset on every shot. The cutter
-- doesn't raycast at all: plants are scene nodes without colliders (see
-- flora_gen), so it takes the nearest plant inside a forgiving aim cone, which
-- is what a gathering verb wants anyway.
--
-- YIELDS. A dab of laser reports what came out of the ground by asking
-- `materials.pickOre` for this world's archetype at this DEPTH, with the roll
-- taken from a smooth noise field at the hit point — so ore comes in veins you
-- can follow rather than a slot machine you can stand still and pull. Depth is
-- measured honestly: a second ray from above the surface, so a shaft's tenth
-- metre really is ten metres down.
--
-- This supersedes `dig_tool` in the game scene (that script stays as the small
-- Terrain-API demo the planetoid scene uses).

defaults = {
  laser_radius = 0.95,
  laser_rate = 0.30,     -- seconds between dabs
  laser_range = 26.0,
  laser_strength = 0.55,
  spade_radius = 1.4,
  spade_rate = 0.14,
  spade_strength = 0.6,
  cut_reach = 4.6,
  cut_cone = 0.55,
}

-- Published: the HUD and the smoke harness read these.
tool = 1
prompt = ""
progress = 0.0

TOOLS = {
  { id = "laser", label = "Mining Laser", hint = "hold E to cut rock" },
  { id = "cutter", label = "Harvest Cutter", hint = "hold E to harvest" },
  { id = "spade", label = "Terrain Spade", hint = "E lower · Q raise" },
}

local cam
local last_x, last_y, last_z, last_t = nil, nil, nil, -10
local cut_target, cut_work = nil, 0.0
local msg, msg_t = "", -10

local function say(s)
  msg, msg_t = s, time
end

function toolId()
  return (TOOLS[tool] or TOOLS[1]).id
end

-- Are we the one driving? Only the on-foot astronaut carries tools, and never
-- while a facility panel, the ops board or the inventory panel owns the keys.
local function on_foot(node)
  if node.visible == false then return false end
  for _, v in ipairs(findScripts("vessel_controller")) do
    if v.piloting then return false end
  end
  local sc = findScript("ship_controller")
  if sc and sc.piloting then return false end
  local gm = findScript("game_manager")
  if gm and gm.loading then return false end
  local fm = findScript("facility_menu")
  if fm and fm.isOpen and fm.isOpen() then return false end
  local ob = findScript("ops_board")
  if ob and ob.isOpen and ob.isOpen() then return false end
  return true
end

-- The world position of the surface directly above a point, and therefore how
-- far under the crust that point is.
local function depth_at(body, x, y, z)
  local dx, dy, dz = x - body.x, y - body.y, z - body.z
  local r = math.sqrt(dx * dx + dy * dy + dz * dz)
  if r < 1e-3 then return 0 end
  local ux, uy, uz = dx / r, dy / r, dz / r
  local top = 120.0
  local hit = raycast(x + ux * top, y + uy * top, z + uz * top, -ux, -uy, -uz, top + 4.0)
  if not hit then return 0 end
  local hr = math.sqrt((hit.x - body.x) ^ 2 + (hit.y - body.y) ^ 2 + (hit.z - body.z) ^ 2)
  return math.max(0, hr - r)
end

-- ── the laser ───────────────────────────────────────────────────────────────

local function fire_laser(node, ox, oy, oz, dx, dy, dz)
  local h = raycast(ox, oy, oz, dx, dy, dz, params.laser_range, node)
  if not h then
    prompt = "no target"
    return
  end
  -- The beam is drawn every frame it's firing; the CUT is spaced, like the
  -- editor brush, so holding the button carves steadily instead of explosively.
  draw.line(ox, oy, oz, h.x, h.y, h.z, 1.0, 0.35, 0.15, 0.9)
  draw.line(ox, oy, oz, h.x, h.y, h.z, 1.0, 0.85, 0.5, 0.45)
  if time - last_t < params.laser_rate then return end
  last_t = time
  last_x, last_y, last_z = h.x, h.y, h.z

  terrain.dig(h.x, h.y, h.z, params.laser_radius, params.laser_strength)

  local body = space.body(space.dominant(h.x, h.y, h.z) or "")
  local inv = findScript("inventory")
  local mats = findScript("materials")
  local cl = findScript("climate")
  if not (body and inv and mats) then return end
  local w = cl and cl.worldOf(body.name)
  local depth = depth_at(body, h.x, h.y, h.z)
  -- The ore field: smooth noise, seeded per world, so a seam is a PLACE. Two
  -- octaves — the coarse one decides the province, the fine one the pocket.
  local seed = (w and w.seed or 1) % 4096
  local n1 = math.noise(h.x * 0.035, h.y * 0.035, h.z * 0.035, seed)
  local n2 = math.noise(h.x * 0.19, h.y * 0.19, h.z * 0.19, seed + 3)
  -- Wrapped rather than clamped: a sum of noise octaves piles up in the middle
  -- of its range, and a middle-heavy roll makes the FIRST and LAST bands of a
  -- yield table (the filler and the rare seam) nearly unreachable. Taking the
  -- fractional part keeps veins contiguous — the field still varies smoothly,
  -- it just wraps at the province edges — while covering the table evenly.
  local mix = (n1 * 0.62 + n2 * 0.38) * 2.5 + 0.5
  local roll = mix - math.floor(mix)
  local mat = mats.pickOre(w and w.kind or "barren", depth, roll)
  local units = 1
  if n2 > 0.45 then units = 2 end          -- a rich pocket pays double
  local took = inv.add("astro", mat, units)
  if took < units then
    prompt = "PACK FULL — " .. inv.line("astro")
    if time - msg_t > 3.0 then say("pack full: " .. inv.line("astro")) end
  else
    prompt = string.format("%s ×%d   ·   %s", mats.name(mat), took, inv.line("astro"))
  end
  if took > 0 then
    spawnEffect("Sparks", h.x, h.y, h.z)
  end
end

-- ── the cutter ──────────────────────────────────────────────────────────────

local function fire_cutter(node, ox, oy, oz, dx, dy, dz, dt, firing)
  local ff = findScript("flora_field")
  if not ff then
    prompt = "no flora here"
    return
  end
  local hit = ff.nearestPlant(ox, oy, oz, dx, dy, dz, params.cut_reach, params.cut_cone)
  if not hit then
    cut_target, cut_work, progress = nil, 0, 0
    prompt = "nothing in reach"
    return
  end
  local sp = hit.rec.sp
  if cut_target ~= hit.pid then
    cut_target, cut_work = hit.pid, 0.0
  end
  if firing then
    cut_work = cut_work + dt
    draw.line(ox, oy, oz, hit.x, hit.y, hit.z, 0.6, 1.0, 0.5, 0.8)
  end
  progress = math.min(1.0, cut_work / math.max(0.1, sp.work))
  prompt = string.format("%s   %d%%", sp.name, math.floor(progress * 100 + 0.5))
  if cut_work >= sp.work then
    local got, dropped = ff.harvest(hit, "astro")
    local mats = findScript("materials")
    local parts = {}
    for _, g in ipairs(got) do
      parts[#parts + 1] = string.format("%s ×%d", (mats and mats.name(g.mat)) or g.mat, g.n)
    end
    say(#parts > 0 and table.concat(parts, ", ") .. (dropped > 0 and "  (pack full)" or "")
      or "pack full — nothing taken")
    spawnEffect("ScrapeDust", hit.x, hit.y, hit.z)
    cut_target, cut_work, progress = nil, 0, 0
  end
end

-- ── the spade ───────────────────────────────────────────────────────────────

local function fire_spade(node, ox, oy, oz, dx, dy, dz, mode)
  local h = raycast(ox, oy, oz, dx, dy, dz, params.laser_range, node)
  if not h then return end
  local moved = true
  if last_x then
    local ddx, ddy, ddz = h.x - last_x, h.y - last_y, h.z - last_z
    moved = (ddx * ddx + ddy * ddy + ddz * ddz) >= (params.spade_radius * 0.45) ^ 2
  end
  if not moved and (time - last_t) < params.spade_rate then return end
  last_x, last_y, last_z, last_t = h.x, h.y, h.z, time
  terrain.sculpt(h.x, h.y, h.z, params.spade_radius, params.spade_strength, mode)
end

-- ── driver ──────────────────────────────────────────────────────────────────

function start(node)
  cam = findScript("planet_camera")
  tool = save.get("tool.equipped") or 1
end

local hud

function update(node, dt)
  prompt = ""
  if not hud then hud = find("Tool HUD") end
  local carrying = on_foot(node)
  if hud then
    -- The HUD line lives here rather than in its own script: the belt already
    -- knows everything it says, and one writer means one place to change it.
    local el = hud:getcomponent("UiElement")
    if el then el.visible = carrying end
    if carrying then
      local l = hudLine()
      if hud.text ~= l then hud.text = l end
    end
  end
  if not carrying then
    cut_target, cut_work, progress = nil, 0, 0
    return
  end
  if not cam then
    cam = findScript("planet_camera")
    if not cam then return end
  end

  for i = 1, #TOOLS do
    if input.pressed(tostring(i)) and tool ~= i then
      tool = i
      save.set("tool.equipped", i)
      say(TOOLS[i].label .. " — " .. TOOLS[i].hint)
      cut_target, cut_work, progress = nil, 0, 0
    end
  end

  local ox, oy, oz = cam.cam_x, cam.cam_y, cam.cam_z
  local dx, dy, dz = cam.fwd_x, cam.fwd_y, cam.fwd_z
  if not (ox and dx) then return end

  local id = toolId()
  local firing = input.key("e")
  if id == "cutter" then
    fire_cutter(node, ox, oy, oz, dx, dy, dz, dt, firing)
  elseif id == "laser" then
    if firing then
      fire_laser(node, ox, oy, oz, dx, dy, dz)
    else
      last_t = -10
      local inv = findScript("inventory")
      prompt = inv and inv.line("astro") or ""
    end
  else
    if firing then
      fire_spade(node, ox, oy, oz, dx, dy, dz, "lower")
    elseif input.key("q") then
      fire_spade(node, ox, oy, oz, dx, dy, dz, "raise")
    else
      last_x, last_t = nil, -10
    end
  end
end

-- The HUD line: what's equipped, what it's pointed at, and the last thing that
-- happened. One line, because the crosshair is where you're looking.
function hudLine()
  local t = TOOLS[tool] or TOOLS[1]
  local inv = findScript("inventory")
  local head = string.format("[%d] %s", tool, t.label)
  if time - msg_t < 3.0 and msg ~= "" then
    return head .. "   ·   " .. msg
  end
  if prompt ~= "" then return head .. "   ·   " .. prompt end
  return head .. "   ·   " .. (inv and inv.line("astro") or t.hint)
end
