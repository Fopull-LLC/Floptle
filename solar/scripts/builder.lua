-- SHIP BUILDER v1.2 (game roadmap SC1) — free placement, free ORIENTATION,
-- radial mounts, precise editing. A ship is just the connected part graph;
-- any part (or the whole ship) moves freely; the blueprint is self-contained
-- (the launch site never needs this registry).
--
--   catalogue button   pick up a part (ghost follows the cursor)
--     click            place — ONLY on a highlighted attach node (green tie
--                      line); hold ALT to place free (red halo = overlapping,
--                      refused). The first part places freely on the pad.
--     R / SHIFT+R      rotate 15° / 5° (yaw)
--     T / SHIFT+T      pitch 90° / 15°     Y / SHIFT+Y  roll 90° / 15°
--                      (build sideways — a pitched pod is a rocket car's nose)
--     ESC              put the part back
--   click a placed part      pick it (and everything stacked on it) back up
--   hover a placed part      arrows nudge it (SHIFT = fine; ↑/↓ vertical,
--                            ←/→ screen-horizontal, ALT+←/→ depth)
--                            R/T/Y rotate it in place
--   DEL (hovering)           scrap a part + its stack
--   G                        grab the WHOLE ship; click sets it down
--   CTRL+Z undo   ·   CTRL+S save   ·   F5 reload
--
-- RADIAL MOUNTS: parts with side nodes (pod, tanks, radial decoupler) show
-- extra rings on their flanks — anything can snap there, side-by-side at the
-- same height: side boosters, outriggers, wheels-on-a-sideways-ship. The
-- radial decoupler is the separator: at staging its whole outboard branch
-- (parent links) kicks away laterally.
--
-- All clicks are edge-detected IN-SCRIPT (input snapshots can serve two
-- frames at uneven fps — raw `input.clicked` double-fires a pickup into an
-- instant re-place, which reads as "selection doesn't work").

defaults = {
  snap_px = 52.0,     -- screen px within which an attach node captures the ghost
  floor_y = 0.11,     -- top of the pad
  nudge = 0.1,        -- precise-edit step (SHIFT = a quarter of this)
}

-- ── Part registry (builder-side only; blueprints embed everything) ──────────
-- h = FULL visual stack height (measured from the mesh AABB × prefab scale,
-- so stacked parts sit flush); rx/rz = half-widths for overlap tests.
-- side = true exposes 4 radial attach nodes on the part's flanks.
local REG = {
  pod       = { prefab = "PartPod",       label = "Pod Mk1",      h = 0.80, rx = 0.50, rz = 0.50, mass = 1.2,  cost = 400, top = true,  bottom = true,  kind = "crewed", side = true },
  chute     = { prefab = "PartChute",     label = "Parachute",    h = 0.61, rx = 0.28, rz = 0.28, mass = 0.1,  cost = 80,  top = false, bottom = true,  kind = "canvas" },
  tankS     = { prefab = "PartTankS",     label = "FT-S Tank",    h = 1.00, rx = 0.50, rz = 0.50, mass = 1.5,  cost = 120, top = true,  bottom = true,  kind = "tank", fuel = 60,  side = true },
  tankM     = { prefab = "PartTankM",     label = "FT-M Tank",    h = 1.50, rx = 0.50, rz = 0.50, mass = 3.0,  cost = 260, top = true,  bottom = true,  kind = "tank", fuel = 150, side = true },
  engineS   = { prefab = "PartEngineS",   label = "Sputter",      h = 1.30, rx = 0.90, rz = 0.90, mass = 0.8,  cost = 150, top = true,  bottom = true,  kind = "engine", thrust = 55,  burn = 0.9 },
  engineM   = { prefab = "PartEngineM",   label = "Anvil",        h = 1.60, rx = 0.90, rz = 0.90, mass = 1.8,  cost = 380, top = true,  bottom = true,  kind = "engine", thrust = 130, burn = 2.0 },
  decoupler = { prefab = "PartDecoupler", label = "Decoupler",    h = 0.25, rx = 0.51, rz = 0.51, mass = 0.15, cost = 60,  top = true,  bottom = true,  kind = "structural", decouple = true },
  -- The RADIAL decoupler mounts on a flank and kicks its outboard branch
  -- away at staging (side boosters). It is itself a side host: booster
  -- stacks attach to its outer face.
  radialDec = { prefab = "PartDecoupler", label = "Radial Decoupler", h = 0.25, rx = 0.51, rz = 0.51, mass = 0.12, cost = 70, top = false, bottom = false, kind = "structural", decouple = true, radial = true, side = true },
  -- Legs pass the stack THROUGH (top AND bottom nodes): tank → legs → engine
  -- is the classic lander sandwich — bottom=false made that unbuildable.
  legs      = { prefab = "PartLegs",      label = "Landing Legs", h = 0.70, rx = 0.60, rz = 0.60, mass = 0.3,  cost = 90,  top = true,  bottom = true,  kind = "structural", legs = true },
}

-- ── State ───────────────────────────────────────────────────────────────────
local parts = {}        -- uid -> { id, def, node, x,y,z, yaw,pitch,roll, parent (uid|nil) }
local next_uid = 1
local ghost = nil       -- { id, def, node, yaw,pitch,roll [, carried, from_uid] }
local undo_stack = {}
local pending_spawns = 0
local hover_uid = nil
local stats_node, hint_node
local hint_t = 0        -- seconds a transient hint stays up

-- Edge-detected mouse + a short cooldown so one physical click never both
-- picks up AND re-places.
local lmb_prev = false
local click_cool = 0

-- Published (builder_camera frames the ship with these).
centerX, centerY, centerZ = 0.0, 1.5, 0.0
partCount = 0

local function hint(msg, secs)
  if hint_node then hint_node.text = msg end
  hint_t = secs or 2.5
end

local HINT_IDLE = "click part = pick up   ·   hover: arrows nudge, R/T/Y rotate   ·   G grab ship   ·   DEL scrap   ·   CTRL+Z undo   ·   CTRL+S save   ·   RMB+WASD fly   ·   F focus"
local HINT_GHOST = "click an attach node to place (green = stack, amber = radial)   ·   ALT = place free   ·   R yaw · T pitch · Y roll   ·   ESC cancel"
local HINT_GRAB = "move the mouse to slide the whole ship   ·   click to set it down"

local function publish_center()
  local n, sx, sy, sz = 0, 0.0, 0.0, 0.0
  for _, p in pairs(parts) do
    n = n + 1; sx = sx + p.x; sy = sy + p.y; sz = sz + p.z
  end
  partCount = n
  if n > 0 then centerX, centerY, centerZ = sx / n, sy / n, sz / n
  else centerX, centerY, centerZ = 0.0, 1.5, 0.0 end
end

-- Everything stacked ON `uid`, transitively (the subtree a pickup carries).
local function subtree(uid)
  local out, grew = { [uid] = true }, true
  while grew do
    grew = false
    for u, p in pairs(parts) do
      if p.parent and out[p.parent] and not out[u] then out[u] = true; grew = true end
    end
  end
  return out
end

local function set_part_pos(p, x, y, z)
  p.x, p.y, p.z = x, y, z
  if p.node then p.node.x, p.node.y, p.node.z = x, y, z end
end

local function set_part_rot(p, yaw, pitch, roll)
  p.yaw, p.pitch, p.roll = yaw, pitch, roll
  if p.node then p.node.yaw, p.node.pitch, p.node.roll = yaw, pitch, roll end
end

-- ── Orientation math ────────────────────────────────────────────────────────
-- Engine euler order is YXZ (yaw, pitch, roll). rot_basis returns the world
-- columns of the part's local X/Y/Z axes; eff_extents the axis-aligned
-- half-extents of the part's (rx, h/2, rz) box under that rotation — what
-- snapping and overlap tests need once parts can lie on their sides.
local function rot_basis(yaw, pitch, roll)
  local cy, sy = math.cos(yaw or 0), math.sin(yaw or 0)
  local cx, sx = math.cos(pitch or 0), math.sin(pitch or 0)
  local cz, sz = math.cos(roll or 0), math.sin(roll or 0)
  local X = { x = cy * cz + sy * sx * sz, y = cx * sz, z = -sy * cz + cy * sx * sz }
  local Y = { x = -cy * sz + sy * sx * cz, y = cx * cz, z = sy * sz + cy * sx * cz }
  local Z = { x = sy * cx, y = -sx, z = cy * cx }
  return X, Y, Z
end

local function eff_extents(def, yaw, pitch, roll)
  local X, Y, Z = rot_basis(yaw, pitch, roll)
  local hx, hy, hz = def.rx, def.h * 0.5, def.rz
  return math.abs(X.x) * hx + math.abs(Y.x) * hy + math.abs(Z.x) * hz,
         math.abs(X.y) * hx + math.abs(Y.y) * hy + math.abs(Z.y) * hz,
         math.abs(X.z) * hx + math.abs(Y.z) * hy + math.abs(Z.z) * hz
end

local function part_extents(p)
  return eff_extents(p.def, p.yaw, p.pitch, p.roll)
end

-- ── Stats + staging readout ─────────────────────────────────────────────────
-- Stages mirror the FLIGHT model exactly: SPACE fires the lowest remaining
-- decoupler and everything at/below it detaches, and only the BOTTOM live
-- stage's engines burn (everything above the next decoupler waits its turn).
-- Stage k = the stack above the k-th decoupler, thrusting on the engines
-- between decouplers k and k+1. Honest TWR (thrust / mass·g) + pooled fuel.
local function stage_lines()
  -- Radial-decoupler branches are SIDE BOOSTERS: they separate laterally as
  -- one group (fired before the axial cuts), so they get their own row and
  -- stay out of the axial y-window accounting.
  local booster_uids = {}
  local n_boost = 0
  for uid, p in pairs(parts) do
    if p.def.radial then
      n_boost = n_boost + 1
      for u in pairs(subtree(uid)) do booster_uids[u] = true end
    end
  end
  local out = ""
  if n_boost > 0 then
    local m, th, fu = 0.0, 0.0, 0
    for u in pairs(booster_uids) do
      local p = parts[u]
      if p then
        m = m + p.def.mass
        fu = fu + (p.def.fuel or 0)
        th = th + (p.def.thrust or 0)
      end
    end
    out = out .. string.format("\nBOOSTERS (×%d):  thrust %d   fuel %d   %.2f t",
      n_boost, th, fu, m)
  end
  local cuts = {}
  for _, p in pairs(parts) do
    if p.def.decouple and not p.def.radial then cuts[#cuts + 1] = p.y end
  end
  table.sort(cuts)
  for k = 0, #cuts do
    local lo = (k == 0) and -math.huge or cuts[k]
    local hi = cuts[k + 1] or math.huge
    local m, th, fu = 0.0, 0.0, 0
    for uid, p in pairs(parts) do
      if not booster_uids[uid] and p.y > lo + 0.01 then
        m = m + p.def.mass
        fu = fu + (p.def.fuel or 0)
        if (p.def.thrust or 0) > 0 and p.y < hi - 0.01 then
          th = th + p.def.thrust
        end
      end
    end
    if k == 0 or th > 0 or fu > 0 then
      out = out .. string.format("\nSTAGE %d:  TWR %.2f   fuel %d   %.2f t",
        k + 1, (m > 0 and th > 0) and th / (m * 9.81) or 0, fu, m)
    end
  end
  return out
end

local function refresh_stats()
  if not stats_node then return end
  local mass, cost, thrust, n = 0.0, 0, 0.0, 0
  for _, p in pairs(parts) do
    n = n + 1
    mass = mass + p.def.mass
    cost = cost + p.def.cost
    if p.def.thrust then thrust = thrust + p.def.thrust end
  end
  if n == 0 then
    stats_node.text = "empty pad — pick a part from the catalogue"
    return
  end
  local twr = (mass > 0) and (thrust / (mass * 9.81)) or 0
  local twr_s = (thrust > 0) and string.format("   TWR %.2f", twr) or ""
  stats_node.text = string.format("%d parts   %.2f t   $%d%s%s",
    n, mass, cost, twr_s, stage_lines())
end

-- ── Undo (robust: ops that can't apply yet re-push and wait) ────────────────
local function push_undo(op)
  undo_stack[#undo_stack + 1] = op
  if #undo_stack > 40 then table.remove(undo_stack, 1) end
end

local function spawn_part(id, x, y, z, yaw, parent, uid, pitch, roll, att)
  local def = REG[id]
  local u = uid or next_uid
  if not uid then next_uid = next_uid + 1 end
  pending_spawns = pending_spawns + 1
  spawn(def.prefab, vec3(x, y, z), function(node)
    node.yaw, node.pitch, node.roll = yaw or 0, pitch or 0, roll or 0
    parts[u] = { id = id, def = def, node = node, x = x, y = y, z = z,
                 yaw = yaw or 0, pitch = pitch or 0, roll = roll or 0,
                 parent = parent, att = att }
    pending_spawns = pending_spawns - 1
    publish_center(); refresh_stats()
  end)
  return u
end

local function remove_part(uid)
  local p = parts[uid]
  if not p then return nil end
  local data = { uid = uid, id = p.id, x = p.x, y = p.y, z = p.z, yaw = p.yaw,
                 pitch = p.pitch, roll = p.roll, parent = p.parent }
  if p.node then destroy(p.node) end
  parts[uid] = nil
  for _, q in pairs(parts) do
    if q.parent == uid then q.parent = nil end
  end
  publish_center(); refresh_stats()
  return data
end

local function undo()
  if pending_spawns > 0 then return end -- let in-flight spawns land first
  local op = table.remove(undo_stack)
  if not op then return end
  if op.type == "place" then
    if not remove_part(op.uid) then return end
  elseif op.type == "scrap" then
    for _, d in ipairs(op.parts) do
      spawn_part(d.id, d.x, d.y, d.z, d.yaw, d.parent, d.uid, d.pitch, d.roll)
      if d.uid >= next_uid then next_uid = d.uid + 1 end
    end
  elseif op.type == "move" then
    for _, m in ipairs(op.moved) do
      local p = parts[m.uid]
      if p then
        set_part_pos(p, m.x, m.y, m.z)
        if m.yaw then set_part_rot(p, m.yaw, m.pitch or 0, m.roll or 0) end
        p.parent = m.parent
      end
    end
  end
  publish_center(); refresh_stats()
end

-- ── Picking & snapping ──────────────────────────────────────────────────────
local function screen_of(x, y, z)
  local sx, sy, _, on = camera.worldToScreen(x, y, z)
  return sx, sy, on
end

local function part_under_cursor(px)
  local mx, my = input.mouse()
  -- Precise first: a ray through the cursor against the parts' hulls.
  local ox, oy, oz, dx, dy, dz = camera.screenToRay(mx, my)
  local hit = raycast(ox, oy, oz, dx, dy, dz, 300.0)
  if hit and hit.node then
    for uid, p in pairs(parts) do
      if p.node and p.node.id == hit.node.id then return uid end
    end
  end
  -- Fallback: nearest projected center (small parts between big ones).
  local best, best_d = nil, px * px
  for uid, p in pairs(parts) do
    local sx, sy, on = screen_of(p.x, p.y, p.z)
    if on then
      local d = (sx - mx) ^ 2 + (sy - my) ^ 2
      if d < best_d then best, best_d = uid, d end
    end
  end
  return best
end

-- In-game telegraphs (draw.* — always visible, unlike the editor's debug
-- gizmo layer). Rings mark attach nodes; boxes outline parts.
local function outline(p, r, g, b, a)
  local ex, ey, ez = part_extents(p)
  draw.box(p.x, p.y, p.z, ex + 0.06, ey + 0.06, ez + 0.06,
           p.yaw or 0, r, g, b, a or 1.0)
end

-- Every free attach node of part `p` for the current ghost: axial top/bottom
-- (along the part's OWN axis — a pitched part's stack continues sideways) +
-- 4 radial side nodes on `side = true` parts. Each node carries the world
-- point where the GHOST CENTER would land, so snapping is one lookup.
local function is_side_occupied(px, py, pz, exclude)
  for uid, q in pairs(parts) do
    if not (exclude and exclude[uid]) then
      local ex, ey, ez = part_extents(q)
      if math.abs(px - q.x) < ex * 0.7 and math.abs(py - q.y) < ey * 0.7
        and math.abs(pz - q.z) < ez * 0.7 then
        return true
      end
    end
  end
  return false
end

local function attach_nodes_of(uid, p, gdef, g_ext_y, exclude, occupied_top, occupied_bottom)
  local out = {}
  local X, Y, Z = rot_basis(p.yaw, p.pitch, p.roll)
  local hh = p.def.h * 0.5
  if p.def.top and gdef.bottom and not occupied_top[uid] then
    local ax, ay, az = p.x + Y.x * hh, p.y + Y.y * hh, p.z + Y.z * hh
    out[#out + 1] = { uid = uid, side = "top", x = ax, y = ay, z = az,
      cx = ax + Y.x * g_ext_y, cy = ay + Y.y * g_ext_y, cz = az + Y.z * g_ext_y }
  end
  if p.def.bottom and gdef.top and not occupied_bottom[uid] then
    local ax, ay, az = p.x - Y.x * hh, p.y - Y.y * hh, p.z - Y.z * hh
    out[#out + 1] = { uid = uid, side = "bottom", x = ax, y = ay, z = az,
      cx = ax - Y.x * g_ext_y, cy = ay - Y.y * g_ext_y, cz = az - Y.z * g_ext_y }
  end
  -- Radial flanks: ghost sits side-by-side at the same height, its center
  -- pushed out by host half-width + the ghost's own half-width that way.
  if p.def.side then
    local gex, gey, gez = eff_extents(gdef, ghost.yaw, ghost.pitch, ghost.roll)
    local dirs = {
      { a = X, r = p.def.rx },
      { a = { x = -X.x, y = -X.y, z = -X.z }, r = p.def.rx },
      { a = Z, r = p.def.rz },
      { a = { x = -Z.x, y = -Z.y, z = -Z.z }, r = p.def.rz },
    }
    for _, d in ipairs(dirs) do
      -- Ghost half-width projected along the outward direction.
      local g = math.abs(d.a.x) * gex + math.abs(d.a.y) * gey + math.abs(d.a.z) * gez
      local ax = p.x + d.a.x * d.r
      local ay = p.y + d.a.y * d.r
      local az = p.z + d.a.z * d.r
      local cx, cy, cz = ax + d.a.x * g, ay + d.a.y * g, az + d.a.z * g
      if not is_side_occupied(cx, cy, cz, exclude) then
        out[#out + 1] = { uid = uid, side = "radial", x = ax, y = ay, z = az,
          cx = cx, cy = cy, cz = cz, dx = d.a.x, dy = d.a.y, dz = d.a.z }
      end
    end
  end
  return out
end

-- The best FREE attach node for the current ghost (nearest to the cursor on
-- screen, within snap_px). Carried stacks never attach to themselves.
local function find_snap()
  if not ghost then return nil end
  local gdef = ghost.def
  local mx, my = input.mouse()
  local best, best_d = nil, params.snap_px * params.snap_px
  local exclude = {}
  if ghost.carried then
    for _, m in ipairs(ghost.carried) do exclude[m.uid] = true end
  end
  local occupied_top, occupied_bottom = {}, {}
  for _, p in pairs(parts) do
    if p.parent and parts[p.parent] and not exclude[p.parent] and p.att ~= "radial" then
      if p.y >= parts[p.parent].y then occupied_top[p.parent] = true
      else occupied_bottom[p.parent] = true end
    end
  end
  local _, g_ext_y = eff_extents(gdef, ghost.yaw, ghost.pitch, ghost.roll)
  for uid, p in pairs(parts) do
    if not exclude[uid] then
      for _, a in ipairs(attach_nodes_of(uid, p, gdef, g_ext_y, exclude,
                                         occupied_top, occupied_bottom)) do
        local sx, sy, on = screen_of(a.x, a.y, a.z)
        if on then
          local d = (sx - mx) ^ 2 + (sy - my) ^ 2
          if d < best_d then best, best_d = a, d end
        end
      end
    end
  end
  return best
end

-- Would a part of `def` (at the ghost's orientation) centered at (x,y,z)
-- overlap any placed part's box? Rotated parts test with their effective
-- axis-aligned extents. (Boxes shrunk a touch so flush stacking never
-- counts as overlap.)
local function overlaps(def, x, y, z, exclude, gy, gp, gr)
  local ax, ay, az = eff_extents(def, gy, gp, gr)
  for uid, p in pairs(parts) do
    if not (exclude and exclude[uid]) then
      local bx, by, bz = part_extents(p)
      if math.abs(x - p.x) < (ax + bx) * 0.9
        and math.abs(y - p.y) < (ay + by) * 0.9
        and math.abs(z - p.z) < (az + bz) * 0.9 then
        return true
      end
    end
  end
  return false
end

-- Where a free-floating ghost sits: cursor ray onto the pad plane, else held
-- at the ship's depth.
local function free_pos()
  local mx, my = input.mouse()
  local ox, oy, oz, dx, dy, dz = camera.screenToRay(mx, my)
  local _, gey = eff_extents(ghost.def, ghost.yaw, ghost.pitch, ghost.roll)
  local gh = gey + params.floor_y
  if dy < -1e-4 then
    local t = (gh - oy) / dy
    if t > 0.3 and t < 400 then return ox + dx * t, gh, oz + dz * t end
  end
  local ddx, ddy, ddz = centerX - ox, centerY - oy, centerZ - oz
  local t = math.max(3.0, ddx * dx + ddy * dy + ddz * dz)
  return ox + dx * t, math.max(gh, oy + dy * t), oz + dz * t
end

-- Keep an angle readable: wrap to (-π, π].
local function wrap_angle(a)
  while a > math.pi do a = a - 2 * math.pi end
  while a <= -math.pi do a = a + 2 * math.pi end
  return a
end

-- Shared rotation keys: R yaw 15° (SHIFT 5°), T pitch and Y roll in 90°
-- quarter-turns (SHIFT 15°) — the same grammar for the ghost in hand and a
-- placed part under the cursor. Returns true if anything turned.
local function rot_input(obj)
  local turned = false
  if input.pressed("r") then
    local step = input.key("shift") and (math.pi / 36) or (math.pi / 12)
    obj.yaw = wrap_angle((obj.yaw or 0) + step)
    turned = true
  end
  if input.pressed("t") then
    local step = input.key("shift") and (math.pi / 12) or (math.pi / 2)
    obj.pitch = wrap_angle((obj.pitch or 0) + step)
    turned = true
  end
  if input.pressed("y") then
    local step = input.key("shift") and (math.pi / 12) or (math.pi / 2)
    obj.roll = wrap_angle((obj.roll or 0) + step)
    turned = true
  end
  return turned
end

-- ── The catalogue calls this (findScript("builder").pick) ───────────────────
function pick(id)
  if ghost or not REG[id] then return end
  local def = REG[id]
  ghost = { id = id, def = def, yaw = 0, pitch = 0, roll = 0, node = nil }
  click_cool = 0.15
  hint(HINT_GHOST, 6.0)
  spawn(def.prefab, vec3(0, -80, 0), function(node)
    if ghost and ghost.id == id and not ghost.node and not ghost.carried then
      ghost.node = node
    else
      destroy(node)
    end
  end)
end

local function pickup(uid)
  local grab = subtree(uid)
  local moved = {}
  for u in pairs(grab) do
    local p = parts[u]
    moved[#moved + 1] = { uid = u, id = p.id, x = p.x, y = p.y, z = p.z,
                          yaw = p.yaw, pitch = p.pitch, roll = p.roll, parent = p.parent }
  end
  local root = parts[uid]
  ghost = { id = root.id, def = root.def, yaw = root.yaw, pitch = root.pitch or 0,
            roll = root.roll or 0, node = root.node, carried = moved, from_uid = uid }
  root.parent = nil
  push_undo({ type = "move", moved = moved })
  click_cool = 0.18
  hint(HINT_GHOST, 6.0)
end

local function place_ghost(x, y, z, parent, att)
  if ghost.carried then
    local root = parts[ghost.from_uid]
    if root then
      local dx, dy, dz = x - root.x, y - root.y, z - root.z
      for _, m in ipairs(ghost.carried) do
        local p = parts[m.uid]
        if p then set_part_pos(p, p.x + dx, p.y + dy, p.z + dz) end
      end
      root.parent = parent
      root.att = att
    end
  else
    -- The ghost node BECOMES the placed part. (Spawning a second node here
    -- was the great twin bug: every placed part got an invisible orphan
    -- double exactly on top of it — phantom "copies", un-selectable spots,
    -- parts "clipping inside each other".)
    local u = next_uid
    next_uid = next_uid + 1
    ghost.node.x, ghost.node.y, ghost.node.z = x, y, z
    ghost.node.yaw, ghost.node.pitch, ghost.node.roll = ghost.yaw, ghost.pitch, ghost.roll
    parts[u] = { id = ghost.id, def = ghost.def, node = ghost.node,
                 x = x, y = y, z = z, yaw = ghost.yaw, pitch = ghost.pitch,
                 roll = ghost.roll, parent = parent, att = att }
    push_undo({ type = "place", uid = u })
  end
  ghost = nil
  click_cool = 0.15
  hint(HINT_IDLE, 0.0); hint_t = 0
  publish_center(); refresh_stats()
end

local function cancel_ghost()
  if not ghost then return end
  if ghost.carried then
    ghost = nil
    undo() -- restores the pre-pickup poses + links (the op pushed at pickup)
  else
    if ghost.node then destroy(ghost.node) end
    ghost = nil
  end
  hint(HINT_IDLE, 0.0); hint_t = 0
end

-- ── Blueprint (self-contained; save.* slot store) ───────────────────────────
local function save_blueprint()
  local bp = { parts = {} }
  local ref_y = math.huge
  for _, p in pairs(parts) do ref_y = math.min(ref_y, p.y - p.def.h * 0.5) end
  if ref_y == math.huge then ref_y = 0 end
  local i = 0
  for uid, p in pairs(parts) do
    i = i + 1
    local d = p.def
    bp.parts[i] = {
      uid = uid, id = p.id, prefab = d.prefab, label = d.label,
      x = p.x - centerX, y = p.y - ref_y, z = p.z - centerZ,
      yaw = p.yaw, pitch = p.pitch or 0, roll = p.roll or 0,
      parent = p.parent or 0, att = p.att or "",
      h = d.h, mass = d.mass, cost = d.cost, kind = d.kind,
      thrust = d.thrust or 0, burn = d.burn or 0, fuel = d.fuel or 0,
      decouple = d.decouple and 1 or 0, legs = d.legs and 1 or 0,
      radial = d.radial and 1 or 0,
    }
  end
  save.set("shipyard.blueprint", bp)
  save.flush()
  hint("blueprint saved  ·  " .. i .. " parts", 2.5)
end

local function load_blueprint()
  local bp = save.get("shipyard.blueprint")
  if not bp or not bp.parts then return end
  for uid in pairs(parts) do remove_part(uid) end
  undo_stack = {}
  for _, d in pairs(bp.parts) do
    spawn_part(d.id, d.x, d.y + params.floor_y, d.z, d.yaw,
               d.parent ~= 0 and d.parent or nil, d.uid, d.pitch, d.roll,
               d.att ~= "" and d.att or nil)
    if d.uid >= next_uid then next_uid = d.uid + 1 end
  end
end

-- ── Lifecycle ───────────────────────────────────────────────────────────────
function start(node)
  stats_node = find("BuildStats")
  hint_node = find("BuildHint")
  load_blueprint()
  refresh_stats()
end

local grab_mode = false
local grab_last = nil
local grab_moved = false
local nudge_uid = nil -- the part a precise-edit undo op is already open for

-- Engineering markers (in-game draw.* layer): the CENTER OF MASS (amber
-- sphere + ring) and CENTER OF THRUST (blue sphere + thrust-axis line).
-- Watching them line up IS the balance tool — an off-axis CoT will pirouette
-- exactly this way at launch, because flight thrusts at these same offsets.
local function draw_engineering()
  if partCount == 0 then return end
  local m, cx, cy, cz = 0.0, 0.0, 0.0, 0.0
  local th, tx, ty, tz = 0.0, 0.0, 0.0, 0.0
  for _, p in pairs(parts) do
    local pm = p.def.mass
    m = m + pm
    cx, cy, cz = cx + p.x * pm, cy + p.y * pm, cz + p.z * pm
    local t = p.def.thrust or 0
    if t > 0 then
      th = th + t
      tx, ty, tz = tx + p.x * t, ty + p.y * t, tz + p.z * t
    end
  end
  if m <= 0 then return end
  cx, cy, cz = cx / m, cy / m, cz / m
  draw.sphere(cx, cy, cz, 0.16, 1.0, 0.8, 0.2, 1.0)
  draw.ring(cx, cy, cz, 0, 1, 0, 0.34, 1.0, 0.8, 0.2, 0.7)
  if th > 0 then
    tx, ty, tz = tx / th, ty / th, tz / th
    draw.sphere(tx, ty, tz, 0.13, 0.3, 0.7, 1.0, 1.0)
    -- The thrust axis (builder ships point +Y): CoT up through the CoM
    -- region — if this line misses the amber ball, the ship will torque.
    draw.line(tx, ty, tz, tx, ty + 2.2, tz, 0.3, 0.7, 1.0, 0.8)
  end
end

function update(node, dt)
  draw_engineering()
  -- One shared click edge for the whole frame.
  local lmb = input.button(0)
  local clicked = lmb and not lmb_prev and click_cool <= 0
  lmb_prev = lmb
  if click_cool > 0 then click_cool = click_cool - dt end
  if hint_t > 0 then
    hint_t = hint_t - dt
    if hint_t <= 0 and hint_node and not ghost and not grab_mode then
      hint_node.text = HINT_IDLE
    end
  end
  local cam_busy = input.button(1) -- RMB = the camera's; never build through it

  -- Self-heal: never let a bad ghost wedge the builder.
  if ghost then
    if ghost.carried and not parts[ghost.from_uid] then
      ghost = nil
      hint(HINT_IDLE, 0.0); hint_t = 0
    elseif not ghost.carried and not ghost.node then
      ghost.wait = (ghost.wait or 0) + dt
      if ghost.wait > 4.0 then
        ghost = nil
        hint("that part failed to spawn — try again", 2.5)
      end
    end
  end

  -- ── Ghost follows the cursor ──
  if ghost and (ghost.node or ghost.carried) then
    local snap = find_snap()
    local x, y, z
    if snap then
      x, y, z = snap.cx, snap.cy, snap.cz -- the ghost-center this node implies
      -- A RADIAL DECOUPLER self-orients on a flank: its disc axis turns to
      -- face outward (that's the direction its branch will kick away).
      if snap.side == "radial" and ghost.def.radial then
        ghost.roll = math.asin(math.max(-1, math.min(1, -snap.dx)))
        ghost.pitch = math.atan2(snap.dz, snap.dy)
        local gex, gey, gez = eff_extents(ghost.def, ghost.yaw, ghost.pitch, ghost.roll)
        local g = math.abs(snap.dx) * gex + math.abs(snap.dy) * gey + math.abs(snap.dz) * gez
        x = snap.x + snap.dx * g
        y = snap.y + snap.dy * g
        z = snap.z + snap.dz * g
      end
    else
      x, y, z = free_pos()
    end
    if ghost.carried then
      local root = parts[ghost.from_uid]
      local dx, dy, dz = x - root.x, y - root.y, z - root.z
      for _, m in ipairs(ghost.carried) do
        local p = parts[m.uid]
        if p then set_part_pos(p, p.x + dx, p.y + dy, p.z + dz) end
      end
    elseif ghost.node then
      ghost.node.x, ghost.node.y, ghost.node.z = x, y, z
      ghost.node.yaw, ghost.node.pitch, ghost.node.roll = ghost.yaw, ghost.pitch, ghost.roll
    end

    -- Telegraphs (in-game draw.* layer): EVERY free attach node shows as a
    -- ring while a part is in hand; the captured one goes bright green with a
    -- tie line; carried stacks get amber outlines.
    local exclude = nil
    if ghost.carried then
      exclude = {}
      for _, m in ipairs(ghost.carried) do
        exclude[m.uid] = true
        local cp = parts[m.uid]
        if cp then outline(cp, 1.0, 0.75, 0.3, 0.9) end
      end
    end
    do
      local occupied_top, occupied_bottom = {}, {}
      for _, p in pairs(parts) do
        if p.parent and parts[p.parent] and not (exclude and exclude[p.parent])
          and p.att ~= "radial" then
          if p.y >= parts[p.parent].y then occupied_top[p.parent] = true
          else occupied_bottom[p.parent] = true end
        end
      end
      local _, gey = eff_extents(ghost.def, ghost.yaw, ghost.pitch, ghost.roll)
      for uid, p in pairs(parts) do
        if not (exclude and exclude[uid]) then
          for _, a in ipairs(attach_nodes_of(uid, p, ghost.def, gey, exclude,
                                             occupied_top, occupied_bottom)) do
            if a.side == "radial" then
              -- Flank nodes ring in AMBER, facing outward.
              draw.ring(a.x, a.y, a.z, a.dx, a.dy, a.dz, 0.22, 1.0, 0.75, 0.3, 0.8)
            else
              draw.ring(a.x, a.y, a.z, 0, 1, 0, 0.3, 0.35, 0.85, 1.0, 0.8)
            end
          end
        end
      end
    end
    local free_ok = (partCount == 0) or (ghost.carried and #ghost.carried >= partCount)
        or input.key("alt")
    local can_place, why
    if snap then
      local ex2 = { [snap.uid] = true }
      if exclude then for u in pairs(exclude) do ex2[u] = true end end
      if overlaps(ghost.def, x, y, z, ex2, ghost.yaw, ghost.pitch, ghost.roll) then
        can_place, why = false, "that spot is blocked by another part"
        draw.sphere(x, y, z, 0.35, 1.0, 0.25, 0.2, 1.0)
      else
        can_place = true
        draw.line(x, y, z, snap.x, snap.y, snap.z, 0.3, 1.0, 0.4, 1.0)
        draw.ring(snap.x, snap.y, snap.z, 0, 1, 0, 0.4, 0.3, 1.0, 0.4, 1.0)
      end
    elseif free_ok then
      if overlaps(ghost.def, x, y, z, exclude, ghost.yaw, ghost.pitch, ghost.roll) then
        can_place, why = false, "overlapping — find clear ground"
        draw.sphere(x, y, z, 0.35, 1.0, 0.25, 0.2, 1.0)
      else
        can_place = true
        local _, gey2 = eff_extents(ghost.def, ghost.yaw, ghost.pitch, ghost.roll)
        draw.ring(x, y - gey2, z, 0, 1, 0, 0.35, 0.4, 0.7, 1.0, 1.0)
      end
    else
      can_place, why = false, nil -- routine: aim at an attach node
      draw.sphere(x, y, z, 0.35, 1.0, 0.25, 0.2, 1.0)
    end

    -- DEL with a part in hand scraps EVERYTHING being carried.
    if input.pressed("delete") or input.pressed("del") then
      if ghost.carried then
        local datas = {}
        for _, m in ipairs(ghost.carried) do
          local d = remove_part(m.uid)
          if d then datas[#datas + 1] = d end
        end
        push_undo({ type = "scrap", parts = datas })
        hint("scrapped " .. #datas .. " carried part(s) — CTRL+Z to undo", 2.5)
      elseif ghost.node then
        destroy(ghost.node)
        hint(HINT_IDLE, 0.0); hint_t = 0
      end
      ghost = nil
      return
    end

    if rot_input(ghost) then
      local gn = ghost.carried and parts[ghost.from_uid].node or ghost.node
      if gn then gn.yaw, gn.pitch, gn.roll = ghost.yaw, ghost.pitch, ghost.roll end
      if ghost.carried then
        local r = parts[ghost.from_uid]
        if r then r.yaw, r.pitch, r.roll = ghost.yaw, ghost.pitch, ghost.roll end
      end
    end
    if input.pressed("escape") then cancel_ghost() return end
    if not cam_busy and clicked then
      if can_place then
        place_ghost(x, y, z, snap and snap.uid or nil, snap and snap.side or nil)
      elseif why then
        hint(why, 2.0)
      else
        hint("no attach node under the cursor — aim at a green node, or hold ALT to place free", 2.5)
      end
    end
    return
  end

  -- ── Whole-ship grab ──
  if input.pressed("g") and partCount > 0 and not grab_mode then
    grab_mode = true
    grab_last = nil
    grab_moved = false
    local moved = {}
    for uid, p in pairs(parts) do
      moved[#moved + 1] = { uid = uid, x = p.x, y = p.y, z = p.z, parent = p.parent }
    end
    push_undo({ type = "move", moved = moved })
    click_cool = 0.15
    hint(HINT_GRAB, 8.0)
  end
  if grab_mode then
    local mx, my = input.mouse()
    local ox, oy, oz, dx, dy, dz = camera.screenToRay(mx, my)
    if dy < -1e-4 then
      local t = (params.floor_y - oy) / dy
      if t > 0.3 and t < 500 then
        local gx, gz = ox + dx * t, oz + dz * t
        if grab_last then
          local ddx, ddz = gx - grab_last.x, gz - grab_last.z
          if math.abs(ddx) + math.abs(ddz) > 1e-4 then grab_moved = true end
          for _, p in pairs(parts) do set_part_pos(p, p.x + ddx, p.y, p.z + ddz) end
          publish_center()
        end
        grab_last = { x = gx, z = gz }
      end
    end
    draw.ring(centerX, params.floor_y + 0.02, centerZ, 0, 1, 0, 1.2, 0.4, 0.8, 1.0, 1.0)
    if (clicked and not cam_busy) or input.pressed("escape") or input.pressed("g") then
      grab_mode = false
      -- A grab that never moved anything shouldn't eat a CTRL+Z.
      if not grab_moved and #undo_stack > 0 and undo_stack[#undo_stack].type == "move" then
        table.remove(undo_stack)
      end
      hint(HINT_IDLE, 0.0); hint_t = 0
    end
    return
  end

  -- ── Hover / precise edit / pickup / scrap ──
  hover_uid = (not cam_busy) and part_under_cursor(70) or nil
  if hover_uid then
    local p = parts[hover_uid]
    -- Selection outline: the hovered part bright, the stack it would carry dim.
    outline(p, 0.55, 0.85, 1.0, 1.0)
    for u in pairs(subtree(hover_uid)) do
      if u ~= hover_uid and parts[u] then outline(parts[u], 0.55, 0.85, 1.0, 0.35) end
    end

    -- PRECISE EDIT: arrows nudge the part + its stack from where it sits
    -- (↑/↓ vertical, ←/→ screen-horizontal snapped to a world axis, ALT+←/→
    -- the depth axis; SHIFT = fine). R/T/Y rotate the part in place. One
    -- undo op per hover streak — CTRL+Z restores the whole adjustment.
    local step = input.key("shift") and params.nudge * 0.25 or params.nudge
    local ddx, ddy, ddz = 0, 0, 0
    if input.pressed("up") then ddy = step end
    if input.pressed("down") then ddy = -step end
    local h = (input.pressed("right") and 1 or 0) - (input.pressed("left") and 1 or 0)
    if h ~= 0 then
      -- Camera-right on the ground, snapped to the nearest world axis, so a
      -- nudge always goes where the arrow points on screen — predictably.
      local mx, my = input.mouse()
      local _, _, _, d1x, _, d1z = camera.screenToRay(mx, my)
      local _, _, _, d2x, _, d2z = camera.screenToRay(mx + 40, my)
      local rxx, rzz = d2x - d1x, d2z - d1z
      if input.key("alt") then rxx, rzz = -rzz, rxx end -- depth axis instead
      if math.abs(rxx) >= math.abs(rzz) then
        ddx = (rxx >= 0 and 1 or -1) * h * step
      else
        ddz = (rzz >= 0 and 1 or -1) * h * step
      end
    end
    local rot = { yaw = p.yaw or 0, pitch = p.pitch or 0, roll = p.roll or 0 }
    local turned = rot_input(rot)
    if ddx ~= 0 or ddy ~= 0 or ddz ~= 0 or turned then
      if nudge_uid ~= hover_uid then
        nudge_uid = hover_uid
        local moved = {}
        for u in pairs(subtree(hover_uid)) do
          local q = parts[u]
          moved[#moved + 1] = { uid = u, x = q.x, y = q.y, z = q.z,
                                yaw = q.yaw, pitch = q.pitch, roll = q.roll, parent = q.parent }
        end
        push_undo({ type = "move", moved = moved })
      end
      if turned then set_part_rot(p, rot.yaw, rot.pitch, rot.roll) end
      if ddx ~= 0 or ddy ~= 0 or ddz ~= 0 then
        for u in pairs(subtree(hover_uid)) do
          local q = parts[u]
          if q then set_part_pos(q, q.x + ddx, q.y + ddy, q.z + ddz) end
        end
        publish_center()
      end
      refresh_stats()
      hint(string.format("precise edit  ·  step %.2g (SHIFT fine)  ·  ALT+←/→ depth  ·  CTRL+Z undoes it all", step), 2.0)
    end

    if clicked then
      pickup(hover_uid)
      return
    end
    if input.pressed("delete") or input.pressed("del") then
      local grab = subtree(hover_uid)
      local datas = {}
      for u in pairs(grab) do
        local d = remove_part(u)
        if d then datas[#datas + 1] = d end
      end
      push_undo({ type = "scrap", parts = datas })
      hint("scrapped " .. #datas .. " part(s) — CTRL+Z to undo", 2.5)
    end
  else
    nudge_uid = nil -- next hover streak opens a fresh undo op
  end

  -- ── Shortcuts ──
  if input.key("ctrl") and input.pressed("z") then undo() end
  if input.key("ctrl") and input.pressed("s") then save_blueprint() end
  if input.pressed("f5") then load_blueprint() end
end

-- The HUD buttons call these too.
function doSave() save_blueprint() end
function doLaunch()
  if partCount == 0 then hint("nothing to launch — build something first", 2.5) return end
  save_blueprint()
  save.set("shipyard.launch", 1)
  save.set("shipyard.pilot", 1) -- you launch IN the pod, not beside it
  save.flush()
  scene.load("system")
end
