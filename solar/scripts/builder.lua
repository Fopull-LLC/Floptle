-- SHIP BUILDER v1 (game roadmap SC1) — free placement, no root-pod tyranny.
-- A ship is just the connected part graph; any part (or the whole ship) moves
-- freely; the blueprint is self-contained (the launch site never needs this
-- registry).
--
--   click a CATALOGUE button   pick up a part (ghost follows the cursor)
--       click                  place it (snaps to the highlighted attach node;
--                              green ghost line = will attach, amber = floating)
--       R / SHIFT+R            rotate the ghost 15° / fine
--       ESC                    put the part back
--   click a PLACED part        pick it (and everything stacked on it) back up
--   DEL (hovering a part)      scrap it + its stack (refunds nothing — undo!)
--   G                          grab the WHOLE ship, click to set it down
--   CTRL+Z                     undo (place / scrap / move / grab)
--   CTRL+S                     save the blueprint    F5  reload it
--
-- Published (read by builder_camera + the HUD): centerX/Y/Z, partCount,
-- pick(id) — the catalogue buttons call pick.

defaults = {
  snap_px = 46.0,     -- screen px within which an attach node captures the ghost
  floor_y = 0.1,      -- top of the work floor
}

-- ── Part registry (builder-side only; blueprints embed everything) ──────────
-- h = stack height; top/bottom = stack attach nodes; engine parts carry
-- thrust (kN-ish) + burn, tanks carry fuel — the stats readout and (later)
-- the flight spawner read these OUT OF THE BLUEPRINT, not from here.
local REG = {
  pod       = { prefab = "PartPod",       label = "Pod Mk1",     h = 1.7, mass = 1.2, cost = 400, top = true,  bottom = true, kind = "crewed" },
  chute     = { prefab = "PartChute",     label = "Parachute",   h = 0.55, mass = 0.1, cost = 80,  top = false, bottom = true, kind = "canvas" },
  tankS     = { prefab = "PartTankS",     label = "FT-S Tank",   h = 1.55, mass = 1.5, cost = 120, top = true,  bottom = true, kind = "tank", fuel = 60 },
  tankM     = { prefab = "PartTankM",     label = "FT-M Tank",   h = 2.35, mass = 3.0, cost = 260, top = true,  bottom = true, kind = "tank", fuel = 150 },
  engineS   = { prefab = "PartEngineS",   label = "Sputter",     h = 1.05, mass = 0.8, cost = 150, top = true,  bottom = false, kind = "engine", thrust = 55,  burn = 0.9 },
  engineM   = { prefab = "PartEngineM",   label = "Anvil",       h = 1.45, mass = 1.8, cost = 380, top = true,  bottom = false, kind = "engine", thrust = 130, burn = 2.0 },
  decoupler = { prefab = "PartDecoupler", label = "Decoupler",   h = 0.25, mass = 0.15, cost = 60, top = true,  bottom = true, kind = "structural", decouple = true },
  legs      = { prefab = "PartLegs",      label = "Landing Legs", h = 0.85, mass = 0.3, cost = 90, top = true,  bottom = false, kind = "structural", legs = true },
}

-- ── State ───────────────────────────────────────────────────────────────────
local parts = {}        -- uid -> { id, def, node, x,y,z, yaw, parent (uid|nil) }
local next_uid = 1
local ghost = nil       -- { uid?, id, def, node, yaw, restore? } while placing
local undo_stack = {}
local hover_uid = nil
local snap_target = nil -- { uid, side = "top"|"bottom" } for the current ghost
local cam
local stats_node, hint_node

-- Published (builder_camera frames the ship with these).
centerX, centerY, centerZ = 0.0, 1.5, 0.0
partCount = 0

local function iter_parts()
  return pairs(parts)
end

local function publish_center()
  local n, sx, sy, sz = 0, 0.0, 0.0, 0.0
  for _, p in iter_parts() do
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
    for u, p in iter_parts() do
      if p.parent and out[p.parent] and not out[u] then out[u] = true; grew = true end
    end
  end
  return out
end

local function set_part_pos(p, x, y, z)
  p.x, p.y, p.z = x, y, z
  if p.node then p.node.x, p.node.y, p.node.z = x, y, z end
end

-- ── Stats (honest numbers §4.2 — v1: mass / cost / TWR vs 9.81) ─────────────
local function refresh_stats()
  if not stats_node then return end
  local mass, cost, thrust, n = 0.0, 0, 0.0, 0
  for _, p in iter_parts() do
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
  stats_node.text = string.format("%d parts   %.2f t   $%d%s", n, mass, cost, twr_s)
end

-- ── Undo ────────────────────────────────────────────────────────────────────
local function push_undo(op)
  undo_stack[#undo_stack + 1] = op
  if #undo_stack > 40 then table.remove(undo_stack, 1) end
end

local function spawn_part(id, x, y, z, yaw, parent, uid)
  local def = REG[id]
  local u = uid or next_uid
  if not uid then next_uid = next_uid + 1 end
  spawn(def.prefab, vec3(x, y, z), function(node)
    node.yaw = yaw or 0
    parts[u] = { id = id, def = def, node = node, x = x, y = y, z = z, yaw = yaw or 0, parent = parent }
    publish_center(); refresh_stats()
  end)
  return u
end

local function remove_part(uid)
  local p = parts[uid]
  if not p then return nil end
  local data = { uid = uid, id = p.id, x = p.x, y = p.y, z = p.z, yaw = p.yaw, parent = p.parent }
  if p.node then destroy(p.node) end
  parts[uid] = nil
  -- Orphan anything that was stacked on it (they keep their place, just unlinked).
  for _, q in iter_parts() do
    if q.parent == uid then q.parent = nil end
  end
  publish_center(); refresh_stats()
  return data
end

local function undo()
  local op = table.remove(undo_stack)
  if not op then return end
  if op.type == "place" then
    remove_part(op.uid)
  elseif op.type == "scrap" then
    for _, d in ipairs(op.parts) do
      spawn_part(d.id, d.x, d.y, d.z, d.yaw, d.parent, d.uid)
      if d.uid >= next_uid then next_uid = d.uid + 1 end
    end
  elseif op.type == "move" then
    for _, m in ipairs(op.moved) do
      local p = parts[m.uid]
      if p then set_part_pos(p, m.x, m.y, m.z); p.parent = m.parent end
    end
  end
  publish_center(); refresh_stats()
end

-- ── Picking & snapping ──────────────────────────────────────────────────────
local function screen_of(x, y, z)
  local sx, sy, _, on = camera.worldToScreen(x, y, z)
  return sx, sy, on
end

-- The placed part whose center is nearest the cursor (within `px`), for
-- pickup / hover / delete.
local function part_under_cursor(px)
  local mx, my = input.mouse()
  local best, best_d = nil, px * px
  for uid, p in iter_parts() do
    local sx, sy, on = screen_of(p.x, p.y, p.z)
    if on then
      local d = (sx - mx) ^ 2 + (sy - my) ^ 2
      if d < best_d then best, best_d = uid, d end
    end
  end
  return best
end

-- The best attach node for the current ghost: scan every placed part's free
-- stack nodes, project to screen, take the nearest to the cursor within
-- snap_px. A node is FREE if no part is already linked there.
local function find_snap()
  if not ghost then return nil end
  local gdef = ghost.def
  local mx, my = input.mouse()
  local best, best_d = nil, params.snap_px * params.snap_px
  -- A carried stack must never attach to itself.
  local exclude = {}
  if ghost.carried then
    for _, m in ipairs(ghost.carried) do exclude[m.uid] = true end
  end
  local occupied_top, occupied_bottom = {}, {}
  for _, p in iter_parts() do
    if p.parent then
      -- p sits ON its parent: if p is ABOVE the parent it occupies the top.
      local pp = parts[p.parent]
      if pp then
        if p.y >= pp.y then occupied_top[p.parent] = true
        else occupied_bottom[p.parent] = true end
      end
    end
  end
  for uid, p in iter_parts() do
    if exclude[uid] then goto continue end
    -- Their TOP welcomes our BOTTOM; their BOTTOM welcomes our TOP.
    if p.def.top and gdef.bottom and not occupied_top[uid] then
      local ax, ay, az = p.x, p.y + p.def.h * 0.5, p.z
      local sx, sy, on = screen_of(ax, ay, az)
      if on then
        local d = (sx - mx) ^ 2 + (sy - my) ^ 2
        if d < best_d then best, best_d = { uid = uid, side = "top", x = ax, y = ay, z = az }, d end
      end
    end
    if p.def.bottom and gdef.top and not occupied_bottom[uid] then
      local ax, ay, az = p.x, p.y - p.def.h * 0.5, p.z
      local sx, sy, on = screen_of(ax, ay, az)
      if on then
        local d = (sx - mx) ^ 2 + (sy - my) ^ 2
        if d < best_d then best, best_d = { uid = uid, side = "bottom", x = ax, y = ay, z = az }, d end
      end
    end
    ::continue::
  end
  return best
end

-- Where the free-floating ghost sits: the cursor ray dropped onto the ground
-- plane (first parts), or held at the ship's depth once something exists.
local function free_pos()
  local mx, my = input.mouse()
  local ox, oy, oz, dx, dy, dz = camera.screenToRay(mx, my)
  local gh = ghost.def.h * 0.5 + params.floor_y
  -- Prefer the ground plane while it's in front of us.
  if dy < -1e-4 then
    local t = (gh - oy) / dy
    if t > 0.3 and t < 400 then return ox + dx * t, gh, oz + dz * t end
  end
  -- Otherwise hold at the ship's distance along the ray.
  local ddx, ddy, ddz = centerX - ox, centerY - oy, centerZ - oz
  local t = math.max(3.0, ddx * dx + ddy * dy + ddz * dz)
  return ox + dx * t, math.max(gh, oy + dy * t), oz + dz * t
end

-- ── The catalogue calls this (via findScript("builder").pick) ───────────────
function pick(id)
  if ghost or not REG[id] then return end
  local def = REG[id]
  ghost = { id = id, def = def, yaw = 0, node = nil }
  spawn(def.prefab, vec3(0, -50, 0), function(node)  -- parked till first update
    if ghost and ghost.id == id and not ghost.node then ghost.node = node
    else destroy(node) end
  end)
end

-- Pick a PLACED part (and its stack) back up: scrap the subtree into ghost
-- restore data, keep only the picked part visible as the new ghost.
local function pickup(uid)
  local grab = subtree(uid)
  local moved = {}
  for u in pairs(grab) do
    local p = parts[u]
    moved[#moved + 1] = { uid = u, id = p.id, x = p.x, y = p.y, z = p.z, yaw = p.yaw, parent = p.parent,
                          dx = p.x - parts[uid].x, dy = p.y - parts[uid].y, dz = p.z - parts[uid].z }
  end
  local root = parts[uid]
  ghost = { id = root.id, def = root.def, yaw = root.yaw, node = root.node, carried = moved, from_uid = uid }
  -- Detach the root from the graph; carried parts stay live and ride along.
  root.picked = true
  push_undo({ type = "move", moved = moved })
end

local function place_ghost(x, y, z, parent)
  if ghost.carried then
    -- Re-place the whole carried stack, offset from the root's new spot.
    local dx, dy, dz = x - parts[ghost.from_uid].x, y - parts[ghost.from_uid].y, z - parts[ghost.from_uid].z
    for _, m in ipairs(ghost.carried) do
      local p = parts[m.uid]
      if p then set_part_pos(p, p.x + dx, p.y + dy, p.z + dz) end
    end
    parts[ghost.from_uid].parent = parent
    parts[ghost.from_uid].picked = nil
  else
    local uid = spawn_part(ghost.id, x, y, z, ghost.yaw, parent)
    push_undo({ type = "place", uid = uid })
  end
  ghost = nil
  publish_center(); refresh_stats()
end

local function cancel_ghost()
  if not ghost then return end
  if ghost.carried then
    ghost.node = nil -- the part was never removed; it just stops following
    parts[ghost.from_uid].picked = nil
    undo() -- restore the pre-pickup positions (the pushed move op)
  elseif ghost.node then
    destroy(ghost.node)
  end
  ghost = nil
end

-- ── Blueprint (self-contained; save.* slot store) ───────────────────────────
local function save_blueprint()
  local bp = { parts = {} }
  local ref_y = math.huge
  for _, p in iter_parts() do ref_y = math.min(ref_y, p.y - p.def.h * 0.5) end
  if ref_y == math.huge then ref_y = 0 end
  local i = 0
  for uid, p in iter_parts() do
    i = i + 1
    local d = p.def
    bp.parts[i] = {
      uid = uid, id = p.id, prefab = d.prefab, label = d.label,
      x = p.x - centerX, y = p.y - ref_y, z = p.z - centerZ, yaw = p.yaw,
      parent = p.parent or 0,
      h = d.h, mass = d.mass, cost = d.cost, kind = d.kind,
      thrust = d.thrust or 0, burn = d.burn or 0, fuel = d.fuel or 0,
      decouple = d.decouple and 1 or 0, legs = d.legs and 1 or 0,
    }
  end
  save.set("shipyard.blueprint", bp)
  save.flush()
  if hint_node then hint_node.text = "blueprint saved  ·  " .. i .. " parts" end
end

local function load_blueprint()
  local bp = save.get("shipyard.blueprint")
  if not bp or not bp.parts then return end
  for uid in pairs(parts) do remove_part(uid) end
  undo_stack = {}
  for _, d in pairs(bp.parts) do
    spawn_part(d.id, d.x, d.y + params.floor_y, d.z, d.yaw,
               d.parent ~= 0 and d.parent or nil, d.uid)
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

function update(node, dt)
  if not cam then cam = findScript("builder_camera") end
  local cam_busy = input.button(1) -- RMB = camera's; never place through it

  -- ── Ghost follows the cursor ──
  if ghost and ghost.node then
    snap_target = find_snap()
    local x, y, z
    if snap_target then
      local h = ghost.def.h * 0.5
      if snap_target.side == "top" then y = snap_target.y + h else y = snap_target.y - h end
      x, z = snap_target.x, snap_target.z
    else
      x, y, z = free_pos()
    end
    if ghost.carried then
      local dx, dy, dz = x - parts[ghost.from_uid].x, y - parts[ghost.from_uid].y, z - parts[ghost.from_uid].z
      for _, m in ipairs(ghost.carried) do
        local p = parts[m.uid]
        if p then set_part_pos(p, p.x + dx, p.y + dy, p.z + dz) end
      end
    else
      ghost.node.x, ghost.node.y, ghost.node.z = x, y, z
    end
    -- Attach telegraph: green tie line to the captured node, amber halo free.
    if snap_target then
      gizmo.line(x, y, z, snap_target.x, snap_target.y, snap_target.z, 0.3, 1.0, 0.4)
      gizmo.sphere(snap_target.x, snap_target.y, snap_target.z, 0.18, 0.3, 1.0, 0.4)
    else
      gizmo.sphere(x, y, z, 0.22, 1.0, 0.75, 0.25)
    end
    if input.pressed("r") then
      local step = input.key("shift") and (math.pi / 36) or (math.pi / 12)
      ghost.yaw = ghost.yaw + step
      if ghost.node then ghost.node.yaw = ghost.yaw end
    end
    if input.pressed("escape") then cancel_ghost() end
    if not cam_busy and input.clicked(0) then
      place_ghost(x, y, z, snap_target and snap_target.uid or nil)
    end
    return
  end

  -- ── Whole-ship grab ──
  if input.pressed("g") and partCount > 0 then
    grab_mode = not grab_mode
    grab_last = nil
    if grab_mode then
      local moved = {}
      for uid, p in iter_parts() do
        moved[#moved + 1] = { uid = uid, x = p.x, y = p.y, z = p.z, parent = p.parent }
      end
      push_undo({ type = "move", moved = moved })
    end
  end
  if grab_mode then
    local mx, my = input.mouse()
    local ox, oy, oz, dx, dy, dz = camera.screenToRay(mx, my)
    if dy < -1e-4 then
      local t = (centerY - oy) / dy
      if t > 0.3 and t < 500 then
        local gx, gz = ox + dx * t, oz + dz * t
        if grab_last then
          local ddx, ddz = gx - grab_last.x, gz - grab_last.z
          for _, p in iter_parts() do set_part_pos(p, p.x + ddx, p.y, p.z + ddz) end
          publish_center()
        end
        grab_last = { x = gx, z = gz }
      end
    end
    gizmo.sphere(centerX, centerY, centerZ, 0.35, 0.4, 0.8, 1.0)
    if input.clicked(0) or input.pressed("escape") then grab_mode = false end
    return
  end

  -- ── Hover / pickup / scrap ──
  hover_uid = (not cam_busy) and part_under_cursor(60) or nil
  if hover_uid then
    local p = parts[hover_uid]
    gizmo.sphere(p.x, p.y, p.z, 0.28, 0.55, 0.85, 1.0)
    if input.clicked(0) then
      pickup(hover_uid)
      return
    end
    if input.pressed("delete") then
      local grab = subtree(hover_uid)
      local datas = {}
      for u in pairs(grab) do
        local d = remove_part(u)
        if d then datas[#datas + 1] = d end
      end
      push_undo({ type = "scrap", parts = datas })
    end
  end

  -- ── Shortcuts ──
  if input.key("ctrl") and input.pressed("z") then undo() end
  if input.key("ctrl") and input.pressed("s") then save_blueprint() end
  if input.pressed("f5") then load_blueprint() end
end

-- The HUD buttons call these too.
function doSave() save_blueprint() end
function doLaunch()
  save_blueprint()
  save.set("shipyard.launch", 1)
  save.flush()
  scene.load("planetoid")
end
