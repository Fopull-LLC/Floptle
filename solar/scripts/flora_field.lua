-- FLORA FIELD — the living surface. Streams a planet's plants in around
-- whoever is walking (or hovering) on it, and streams them back out behind.
--
-- The world is divided into CELLS on a cube-projected grid around the body, so
-- a cell is a fixed patch of ground with a fixed seed: walk away, walk back, and
-- the same trees stand in the same places. Nothing is stored — the field IS the
-- seed plus the climate at that point — which is what makes a whole planet's
-- forest cost one small table.
--
--   near band   full plants (trunk, branches, foliage)
--   far band    silhouettes (stem + crown) — all you can read at that distance
--   beyond      nothing; the cell is dropped and its nodes destroyed
--
-- A hard BUDGET caps how many plants are alive at once. These are ordinary
-- scene nodes (see flora_gen's note): the engine has no instanced scatter yet
-- (floptle/0036), so the field is deliberately a moving bubble rather than a
-- horizon full of trees. When the engine grows real scatter, this script keeps
-- its job — deciding what grows where — and hands the drawing over.
--
-- HARVEST: `nearestPlant` + `harvest` are the tool-facing API. A cut plant is
-- remembered for `regrow` seconds and then comes back, because the alternative
-- is an unbounded list of every plant you ever cut in the save file. Regrowth
-- is the honest version of that constraint, and it means a base's surroundings
-- recover if you leave them alone.

defaults = {
  near = 55.0,      -- full-detail radius
  far = 115.0,      -- silhouette radius
  cell = 20.0,      -- ground cell size (m)
  spacing = 9.0,    -- metres per plant at density 1.0
  budget = 90,      -- live plants, hard cap
  cells_per_tick = 1,
  interval = 0.25,  -- seconds between field updates
  regrow = 900.0,   -- seconds before a cut plant grows back
  alt_max = 260.0,  -- above this nothing scatters (you're flying, not walking)
  enabled = 1,
}

-- Published for the HUD / smoke harness.
count = 0
bodyName = nil

local live = {}    -- pid → { node, sp, rx, ry, rz, seedn, cell }
local cells = {}   -- ckey → { pids = {}, detail, dist }
local cut = {}     -- pid → time it grows back
local next_t = 0
local cur_body = nil

local AX = { { 1, 2, 3 }, { 2, 3, 1 }, { 3, 1, 2 } }  -- face axis, then the two in-plane

local function len(x, y, z) return math.sqrt(x * x + y * y + z * z) end

local function comp(v, i)
  if i == 1 then return v[1] end
  if i == 2 then return v[2] end
  return v[3]
end

-- Body-relative position → (face, u, v) on the cube of half-size R.
local function project(rx, ry, rz, R)
  local v = { rx, ry, rz }
  local face, best = 1, math.abs(rx)
  if math.abs(ry) > best then face, best = 2, math.abs(ry) end
  if math.abs(rz) > best then face, best = 3, math.abs(rz) end
  local a = AX[face]
  local sgn = comp(v, a[1]) >= 0 and 1 or -1
  local t = R / math.max(1e-6, math.abs(comp(v, a[1])))
  return face, sgn, comp(v, a[2]) * t, comp(v, a[3]) * t
end

-- (face, u, v) → a body-relative DIRECTION (unit).
local function unproject(face, sgn, u, v, R)
  local a = AX[face]
  local q = { 0, 0, 0 }
  q[a[1]] = sgn * R
  q[a[2]] = u
  q[a[3]] = v
  local l = len(q[1], q[2], q[3])
  return q[1] / l, q[2] / l, q[3] / l
end

local function ckey(face, sgn, cu, cv)
  return string.format("%d%s%d,%d", face, sgn > 0 and "+" or "-", cu, cv)
end

-- A stable integer seed for a cell (and, with `i`, for a plant in it).
local function cell_seed(worldSeed, face, sgn, cu, cv, i)
  local s = (worldSeed % 100003) * 31
  s = s + face * 7919 + (sgn > 0 and 104729 or 15485863)
  s = s + (cu % 65536) * 1299709 + (cv % 65536) * 15485867 + (i or 0) * 2654435761
  return math.abs(s % 2147483647) + 1
end

-- ── observers ───────────────────────────────────────────────────────────────
-- Whoever's eyes are on the ground: the astronaut when they're out, otherwise
-- the vessel being flown (so a low pass over a forest actually has a forest).

local function observer()
  local astro = find("Astronaut")
  if astro and astro.visible ~= false then return astro end
  for _, v in ipairs(findScripts("vessel_controller")) do
    if v.piloting and v.node and v.node.valid then return v.node end
  end
  local sc = findScript("ship_controller")
  if sc and sc.piloting then return find("Ship") end
  return astro
end

-- ── the field ───────────────────────────────────────────────────────────────

local function drop_cell(key)
  local c = cells[key]
  if not c then return end
  for _, pid in ipairs(c.pids) do
    local rec = live[pid]
    if rec then
      if rec.node and rec.node.valid then rec.node:destroy() end
      live[pid] = nil
      count = math.max(0, count - 1)
    end
  end
  cells[key] = nil
end

function clearAll()
  for key in pairs(cells) do drop_cell(key) end
  cells, live, count = {}, {}, 0
end

-- Populate one cell: roll its plants, find the ground under each, and grow
-- whatever the climate there allows.
local function populate(key, face, sgn, cu, cv, detail, body, w, cl, fg)
  local planet = find(body.name)
  if not planet then return end
  local R = w.radius or 100
  local relief = math.max(2.0, w.relief or R * 0.06)
  local r = rng(cell_seed(w.seed or 1, face, sgn, cu, cv))
  local rec_pids = {}

  -- How much grows here at all: the biome under the cell's centre.
  local dx, dy, dz = unproject(face, sgn, (cu + 0.5) * params.cell, (cv + 0.5) * params.cell, R)
  local s0 = cl.sampleRel(w, dx * R, dy * R, dz * R)
  local density = (s0 and s0.density or 0)
  if density <= 0 then
    cells[key] = { pids = rec_pids, detail = detail }
    return
  end
  local area = params.cell * params.cell
  local want = math.floor(area / (params.spacing * params.spacing) * density * r:range(0.6, 1.25) + 0.5)
  want = math.min(want, 8)

  for i = 1, want do
    if count >= params.budget then break end
    local pid = key .. "#" .. i
    local re = cut[pid]
    if not (re and time < re) then
      local u = (cu + r:next()) * params.cell
      local v = (cv + r:next()) * params.cell
      local ux, uy, uz = unproject(face, sgn, u, v, R)
      -- Probe from above the highest possible ground, straight down the radial.
      local top = R + relief * 1.6 + 30
      local ox, oy, oz = body.x + ux * top, body.y + uy * top, body.z + uz * top
      local hit = raycast(ox, oy, oz, -ux, -uy, -uz, relief * 3.5 + 90)
      if hit then
        local hx, hy, hz = hit.x - body.x, hit.y - body.y, hit.z - body.z
        local s = cl.sampleRel(w, hx, hy, hz)
        -- Slope: nothing but moss grows on a cliff. The terrain normal is the
        -- ray's, so this is free.
        local slope = (hit.nx or ux) * ux + (hit.ny or uy) * uy + (hit.nz or uz) * uz
        local under = (w.sea or 0) > 0 and len(hx, hy, hz) < (w.sea or 0)
        if s and slope > 0.62 then
          local choices = fg.speciesIn(body.name, s.biome)
          -- Underwater ground grows only what can live there, and dry ground
          -- grows only what can't drown.
          local pool, total = {}, 0
          for _, c in ipairs(choices) do
            if (c.sp.aquatic == true) == under then
              total = total + c.w
              pool[#pool + 1] = { sp = c.sp, acc = total }
            end
          end
          if total > 0 then
            local pickv = r:next() * total
            local sp = pool[#pool].sp
            for _, c in ipairs(pool) do
              if pickv <= c.acc then sp = c.sp; break end
            end
            local seedn = cell_seed(w.seed or 1, face, sgn, cu, cv, i)
            local rec = { sp = sp, rx = hx, ry = hy, rz = hz, seedn = seedn, cell = key }
            live[pid] = rec
            count = count + 1
            rec_pids[#rec_pids + 1] = pid
            fg.build(planet, sp, hx, hy, hz,
              hit.nx or ux, hit.ny or uy, hit.nz or uz, seedn, detail,
              function(root) rec.node = root end)
          end
        end
      end
    end
  end
  cells[key] = { pids = rec_pids, detail = detail }
end

function update(node, dt)
  if params.enabled == 0 then return end
  if time < next_t then return end
  next_t = time + params.interval

  local gm = findScript("game_manager")
  if gm and gm.loading then return end

  local obs = observer()
  if not obs then return end
  local bname = space.dominant(obs.x, obs.y, obs.z)
  local body = bname and space.body(bname)
  local cl = findScript("climate")
  local fg = findScript("flora_gen")
  if not (body and cl and fg) then
    if cur_body then clearAll(); cur_body = nil end
    return
  end
  if bname ~= cur_body then
    clearAll()
    cur_body = bname
    bodyName = bname
  end

  local w = cl.worldOf(bname)
  if not w or #fg.speciesFor(bname) == 0 then
    if next(cells) then clearAll() end
    return
  end

  -- Are we actually near the ground? One ray answers it, and it also keeps the
  -- field off entirely while you're in orbit.
  local rx, ry, rz = obs.x - body.x, obs.y - body.y, obs.z - body.z
  local rl = len(rx, ry, rz)
  if rl < 1e-3 then return end
  local ux, uy, uz = rx / rl, ry / rl, rz / rl
  local ground = raycast(obs.x, obs.y, obs.z, -ux, -uy, -uz, params.alt_max, obs)
  if not ground then
    if next(cells) then clearAll() end
    return
  end

  local R = w.radius or 100
  local face, sgn, u, v = project(rx, ry, rz, R)
  local cu0, cv0 = math.floor(u / params.cell), math.floor(v / params.cell)
  local reach = math.ceil(params.far / params.cell)

  -- Drop what we've walked away from (a little hysteresis so a cell on the edge
  -- doesn't rebuild every other second).
  for key, c in pairs(cells) do
    if (c.dist or 0) > params.far * 1.25 then drop_cell(key) end
  end

  -- Collect what wants building, then do the CLOSEST first. Sweeping the
  -- neighbourhood in index order builds the far corner first and spends the
  -- whole budget out there — you end up standing in a bare circle looking at a
  -- forest on the horizon.
  local want = {}
  for du = -reach, reach do
    for dv = -reach, reach do
      local cu, cv = cu0 + du, cv0 + dv
      local key = ckey(face, sgn, cu, cv)
      local dirx, diry, dirz = unproject(face, sgn, (cu + 0.5) * params.cell,
        (cv + 0.5) * params.cell, R)
      local cx, cy, cz = dirx * rl - rx, diry * rl - ry, dirz * rl - rz
      local d = len(cx, cy, cz)
      local c = cells[key]
      if c then
        c.dist = d
        -- Coming closer upgrades a silhouette to a real plant; going away does
        -- NOT downgrade it (that churn is visible and buys nothing).
        if c.detail == "far" and d < params.near * 0.85 then
          want[#want + 1] = { key = key, cu = cu, cv = cv, d = d, redo = true }
        end
      elseif d <= params.far then
        want[#want + 1] = { key = key, cu = cu, cv = cv, d = d }
      end
    end
  end
  table.sort(want, function(a, b) return a.d < b.d end)

  -- The farthest thing standing: what a nearer cell evicts when the budget is
  -- full. Without this the bubble freezes wherever it first filled up and the
  -- ground you walk onto stays bare.
  local function farthest()
    local worst, wd = nil, -1
    for key, c in pairs(cells) do
      if (c.dist or 0) > wd and #c.pids > 0 then worst, wd = key, c.dist end
    end
    return worst, wd
  end

  local built = 0
  for _, cand in ipairs(want) do
    if built >= params.cells_per_tick then break end
    if cand.redo then drop_cell(cand.key) end
    if count >= params.budget then
      local worst, wd = farthest()
      if worst and wd > cand.d then drop_cell(worst) end
    end
    if count < params.budget then
      populate(cand.key, face, sgn, cand.cu, cand.cv,
        cand.d < params.near and "full" or "far", body, w, cl, fg)
      if cells[cand.key] then cells[cand.key].dist = cand.d end
      built = built + 1
    end
  end
end

-- ── harvest API (the tool belt calls these) ─────────────────────────────────

-- The plant someone is aiming at: nearest within `reach` whose direction is
-- inside the aim cone. Plants have no colliders, so this is the aim test —
-- and it's the forgiving one anyway, which is right for gathering.
function nearestPlant(x, y, z, dx, dy, dz, reach, cone)
  reach = reach or 4.5
  cone = cone or 0.55
  local body = cur_body and space.body(cur_body)
  if not body then return nil end
  local best, bestScore = nil, -1
  for pid, rec in pairs(live) do
    local wx, wy, wz = body.x + rec.rx, body.y + rec.ry, body.z + rec.rz
    local ex, ey, ez = wx - x, wy - y, wz - z
    local d = len(ex, ey, ez)
    if d > 0.01 and d <= reach then
      local dot = (ex * dx + ey * dy + ez * dz) / d
      if dot >= cone then
        -- Prefer what you're pointing straight at, then what's close.
        local score = dot * 2.0 - d / reach
        if score > bestScore then
          bestScore = score
          best = { pid = pid, rec = rec, dist = d, x = wx, y = wy, z = wz }
        end
      end
    end
  end
  return best
end

-- Cut it down: yields go to `cid`, the plant is destroyed, and the spot is
-- marked to regrow. Returns the list of {mat, n} actually taken and how many
-- units were dropped for want of room.
function harvest(hitrec, cid)
  local rec = hitrec and hitrec.rec
  if not rec then return {}, 0 end
  local fg = findScript("flora_gen")
  local inv = findScript("inventory")
  local got, dropped = {}, 0
  for _, y in ipairs(fg and fg.harvestYield(rec.sp, rec.seedn) or {}) do
    local took = inv and inv.add(cid or "astro", y.mat, y.n) or 0
    if took > 0 then got[#got + 1] = { mat = y.mat, n = took } end
    dropped = dropped + (y.n - took)
  end
  if rec.node and rec.node.valid then rec.node:destroy() end
  live[hitrec.pid] = nil
  count = math.max(0, count - 1)
  cut[hitrec.pid] = time + params.regrow
  local c = cells[rec.cell]
  if c then
    for i, pid in ipairs(c.pids) do
      if pid == hitrec.pid then table.remove(c.pids, i); break end
    end
  end
  return got, dropped
end

-- What's growing around here — the survey readout at a base or a landing site.
function surveyLines(x, y, z, radius)
  radius = radius or 40
  local body = cur_body and space.body(cur_body)
  if not body then return {} end
  local tally, order = {}, {}
  for _, rec in pairs(live) do
    local d = len(body.x + rec.rx - x, body.y + rec.ry - y, body.z + rec.rz - z)
    if d <= radius then
      local n = rec.sp.name
      if not tally[n] then tally[n] = { sp = rec.sp, n = 0 }; order[#order + 1] = n end
      tally[n].n = tally[n].n + 1
    end
  end
  table.sort(order)
  local out = {}
  for _, n in ipairs(order) do
    out[#out + 1] = string.format("%2d × %s", tally[n].n, n)
  end
  return out
end
