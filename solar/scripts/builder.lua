-- SHIP BUILDER v1.3 (game roadmap SC1) — free placement, free ORIENTATION,
-- radial mounts + SYMMETRY, precise editing. A ship is just the connected
-- part graph; any part (or the whole ship) moves freely; the blueprint is
-- self-contained (the launch site never needs this registry).
--
--   catalogue button   pick up a part (ghost follows the cursor)
--     click            place — ONLY on a highlighted attach node (green tie
--                      line); hold ALT to place free (red halo = overlapping,
--                      refused). The first part places freely on the pad.
--     R / SHIFT+R      rotate 15° / 5° (yaw)
--     T / SHIFT+T      pitch 90° / 15°     Y / SHIFT+Y  roll 90° / 15°
--                      (build sideways — a pitched pod is a rocket car's nose)
--     X / SHIFT+X      SYMMETRY ×1 ×2 ×3 ×4 ×6 ×8 — a radial placement
--                      repeats around the host's axis (boosters in one click)
--     ESC              put the part back
--   click a placed part      pick it (and everything stacked on it) back up
--                            (a SYMMETRY member brings its whole ring)
--   hover a placed part      arrows nudge it (SHIFT = fine; ↑/↓ vertical,
--                            ←/→ screen-horizontal, ALT+←/→ depth)
--                            R/T/Y rotate it in place
--   DEL (hovering)           scrap a part + its stack (a symmetry member
--                            scraps the whole ring)
--   G                        grab the WHOLE ship; click sets it down
--   CTRL+Z undo   ·   CTRL+S save   ·   F5 reload
--
-- RADIAL MOUNTS: parts with side nodes (pod, tanks, radial decoupler) show
-- extra rings on their flanks — anything can snap there, side-by-side at the
-- same height: side boosters, outriggers, wheels-on-a-sideways-ship. The
-- radial decoupler is the separator: at staging its whole outboard branch
-- (parent links) kicks away laterally.
--
-- SYMMETRY: with ×N up, one radial placement rings N copies around the host
-- (ghost previews show all of them). Anything you then stack ON a symmetry
-- member — a tank on one radial decoupler, an engine under one booster —
-- AUTO-MIRRORS onto every other member of that ring, however deep the stack
-- goes. Rings are edited as one: pick one up and the ring comes with it,
-- scrap one and the ring goes.
--
-- All clicks are edge-detected IN-SCRIPT (input snapshots can serve two
-- frames at uneven fps — raw `input.clicked` double-fires a pickup into an
-- instant re-place, which reads as "selection doesn't work").

defaults = {
  snap_px = 52.0,     -- screen px within which an attach node captures the ghost
  floor_y = 0.11,     -- top of the pad
  nudge = 0.1,        -- precise-edit step (SHIFT = a quarter of this)
}

-- ── Builder audio (non-spatial UI clicks) ────────────────────────────────────
-- Every build action gets a crisp click on the UI mixer bus. Kenney CC0 clips.
local UI_SFX = {
  place  = "audio/kenney/interface/click_001.ogg",
  pickup = "audio/kenney/interface/pluck_001.ogg",
  scrap  = "audio/kenney/interface/close_001.ogg",
  tool   = "audio/kenney/interface/tick_001.ogg",
  save   = "audio/kenney/interface/confirmation_001.ogg",
  launch = "audio/kenney/interface/confirmation_003.ogg",
}
-- Take a KEY (into UI_SFX), not a clip path, so callers don't each capture
-- UI_SFX as an upvalue — `update` is already near LuaJIT's 60-upvalue ceiling.
local function ui(key, vol)
  if not audio then return end
  audio.play(UI_SFX[key] or key, { track = "UI", volume = vol or 1.0 })
end

-- ── Part registry (builder-side only; blueprints embed everything) ──────────
-- h = FULL visual stack height (measured from the mesh AABB × prefab scale,
-- so stacked parts sit flush); rx/rz = half-widths for overlap tests.
-- side = true exposes 4 radial attach nodes on the part's flanks.
local REG = {
  pod       = { prefab = "PartPod",       label = "Pod Mk1",      h = 0.80, rx = 0.50, rz = 0.50, mass = 1.2,  cost = 400, top = true,  bottom = true,  kind = "crewed", side = true },
  nose      = { prefab = "PartNose",      label = "Nose Cone",    h = 0.90, rx = 0.50, rz = 0.50, mass = 0.4,  cost = 90,  top = false, bottom = true,  kind = "structural", aero = true },
  chute     = { prefab = "PartChute",     label = "Parachute",    h = 0.61, rx = 0.28, rz = 0.28, mass = 0.1,  cost = 80,  top = false, bottom = true,  kind = "canvas", chute = true, side = true },
  tankS     = { prefab = "PartTankS",     label = "FT-S Tank",    h = 1.00, rx = 0.50, rz = 0.50, mass = 1.5,  cost = 120, top = true,  bottom = true,  kind = "tank", fuel = 60,  side = true },
  tankM     = { prefab = "PartTankM",     label = "FT-M Tank",    h = 1.50, rx = 0.50, rz = 0.50, mass = 3.0,  cost = 260, top = true,  bottom = true,  kind = "tank", fuel = 150, side = true },
  engineS   = { prefab = "PartEngineS",   label = "Sputter",      h = 1.30, rx = 0.90, rz = 0.90, mass = 0.8,  cost = 150, top = true,  bottom = true,  kind = "engine", thrust = 55,  burn = 0.9 },
  engineM   = { prefab = "PartEngineM",   label = "Anvil",        h = 1.60, rx = 0.90, rz = 0.90, mass = 1.8,  cost = 380, top = true,  bottom = true,  kind = "engine", thrust = 130, burn = 2.0 },
  -- A fin is a SIDE-MOUNT blade: no axial nodes (never centred), not a side
  -- host itself — it can only attach to another part's flank, where it self-
  -- orients to point outward. Ring it with X symmetry for a proper fin set.
  fins      = { prefab = "PartFins",      label = "Aero Fin",     h = 0.95, rx = 0.35, rz = 0.03, mass = 0.5,  cost = 110, top = false, bottom = false, kind = "structural", aero = true, radial_orient = true },
  battery   = { prefab = "PartBattery",   label = "Battery",      h = 1.00, rx = 0.50, rz = 0.50, mass = 0.6,  cost = 130, top = true,  bottom = true,  kind = "structural", power = true, side = true },
  dish      = { prefab = "PartDish",      label = "Comms Dish",   h = 0.80, rx = 0.40, rz = 0.40, mass = 0.2,  cost = 120, top = true,  bottom = true,  kind = "structural", comms = true, side = true },
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

-- ── Symmetry state ──────────────────────────────────────────────────────────
-- sym_n is the ×N mode (X cycles it); parts placed as a ring share a `sym`
-- group id, and children stacked on a ring member auto-mirror into their own
-- ring (chained groups). Group geometry is DERIVED, never stored: a ring's
-- HUB is the members' common parent (first generation) or, for a chained
-- ring, the hub of the parent ring — so save/load only needs the id.
local SYM_STEPS = { 1, 2, 3, 4, 6, 8 }
local sym_n = 1
local next_sym = 0

-- ── Staging order ───────────────────────────────────────────────────────────
-- Stage ITEMS are separation events: each axial decoupler is one; a radial-
-- decoupler RING (symmetry group) is ONE bundled item. `stage_order` lists
-- item keys in FIRING order (row 1 fires first); drag rows in the STAGING
-- panel to reorder. Persisted per-part (`stage`) in the blueprint — flight
-- fires the events in exactly this order.
local stage_order = {}   -- array of keys: "u<uid>" | "g<sym gid>"
local stage_drag = nil   -- { key, row } while a row is being dragged
local stage_hover = nil  -- stage-panel row the cursor is over (world highlight)
local stage_node2 = nil  -- the StagePanel UI node
local saved_stage = {}   -- uid → stage index from a loaded blueprint

-- ── Tools & selection ───────────────────────────────────────────────────────
-- Clicking a part SELECTS it (it doesn't grab/move) — hovering only previews
-- what a click would select. The active TOOL decides what the selection's
-- gizmo does: 1 = Move (arrows), 2 = Rotate (rings), 3 = Stage (numbered
-- decoupler badges + the staging panel), 4 = Grab (pick the part up to
-- re-attach it elsewhere). Switching tools never changes the selection, and
-- editing never changes the selection — no more "moving the wrong thing".
local TOOLS = { "move", "rotate", "stage", "grab" }
local TOOL_LABEL = { move = "MOVE", rotate = "ROTATE", stage = "STAGE", grab = "GRAB" }
local tool = "move"
local selected_uid = nil

local function hint(msg, secs)
  if hint_node then hint_node.text = msg end
  hint_t = secs or 2.5
end

local HINT_IDLE = "click a part = select   ·   1 Move · 2 Rotate · 3 Stage · 4 Grab   ·   drag the gizmo to edit   ·   X symmetry   ·   G grab ship   ·   DEL scrap   ·   CTRL+Z undo · CTRL+S save   ·   RMB+WASD fly"
local HINT_GHOST = "click an attach node to place (green = stack, amber = radial)   ·   ALT = place free   ·   R yaw · T pitch · Y roll   ·   X symmetry ×N   ·   ESC cancel"
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

-- ── Symmetry geometry ───────────────────────────────────────────────────────
-- Rotate world vector (vx,vy,vz) about the hub part's stack axis by `a`.
local function rot_about(hub, vx, vy, vz, a)
  local e1, ax, e2 = rot_basis(hub.yaw, hub.pitch, hub.roll)
  local d = vx * ax.x + vy * ax.y + vz * ax.z
  local px, py, pz = vx - ax.x * d, vy - ax.y * d, vz - ax.z * d
  local c1 = px * e1.x + py * e1.y + pz * e1.z
  local c2 = px * e2.x + py * e2.y + pz * e2.z
  local ca, sa = math.cos(a), math.sin(a)
  local r1 = c1 * ca - c2 * sa
  local r2 = c1 * sa + c2 * ca
  return e1.x * r1 + e2.x * r2 + ax.x * d,
         e1.y * r1 + e2.y * r2 + ax.y * d,
         e1.z * r1 + e2.z * r2 + ax.z * d
end

-- A part's angle around the hub's axis (for mapping ring member → member).
local function angle_about(hub, p)
  local e1, ax, e2 = rot_basis(hub.yaw, hub.pitch, hub.roll)
  local vx, vy, vz = p.x - hub.x, p.y - hub.y, p.z - hub.z
  local d = vx * ax.x + vy * ax.y + vz * ax.z
  local px, py, pz = vx - ax.x * d, vy - ax.y * d, vz - ax.z * d
  return math.atan2(px * e2.x + py * e2.y + pz * e2.z,
                    px * e1.x + py * e1.y + pz * e1.z)
end

local function group_members(gid)
  local out = {}
  for uid, p in pairs(parts) do
    if p.sym == gid then out[#out + 1] = uid end
  end
  table.sort(out)
  return out
end

-- The ring's HUB part: first generation rings share a parent (the hub);
-- a chained ring (children mirrored onto ring members) inherits the hub of
-- its parents' ring. Bounded walk — malformed links return nil safely.
local function group_hub(gid, depth)
  depth = (depth or 0) + 1
  if depth > 8 then return nil end
  local members = group_members(gid)
  if #members == 0 then return nil end
  local first_parent = parts[members[1]] and parts[members[1]].parent
  if not first_parent then return nil end
  local shared = true
  for _, u in ipairs(members) do
    if not parts[u] or parts[u].parent ~= first_parent then shared = false break end
  end
  if shared then return parts[first_parent] end
  local pp = parts[first_parent]
  if pp and pp.sym then return group_hub(pp.sym, depth) end
  return nil
end

-- The outward self-orientation for a radial decoupler at direction (ux,uy,uz).
local function outward_orient(ux, uy, uz)
  local roll = math.asin(math.max(-1, math.min(1, -ux)))
  local pitch = math.atan2(uz, uy)
  return pitch, roll
end


-- The EDIT SET for a part: its subtree — and, for a symmetry-ring member,
-- every member's subtree. Rings pick up, scrap and highlight as ONE.
local function edit_set(uid)
  local p = parts[uid]
  if not (p and p.sym) then return subtree(uid) end
  local out = {}
  for _, m in ipairs(group_members(p.sym)) do
    for u in pairs(subtree(m)) do out[u] = true end
  end
  return out
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

-- The current stage items: key → { label, uids = {decoupler uids} }.
local function stage_items()
  local items = {}
  for uid, p in pairs(parts) do
    if p.def.decouple then
      if p.def.radial then
        local key = p.sym and ("g" .. p.sym) or ("u" .. uid)
        local it = items[key]
        if not it then
          it = { key = key, uids = {}, ring = true, y = p.y }
          items[key] = it
        end
        it.uids[#it.uids + 1] = uid
        it.y = math.min(it.y, p.y)
      else
        items["u" .. uid] = { key = "u" .. uid, uids = { uid }, ring = false, y = p.y }
      end
    end
  end
  return items
end

-- Reconcile stage_order with the live items: stale keys drop, new keys slot
-- in by the classic default (rings first, then axial bottom-up).
local function sync_stage_order()
  local items = stage_items()
  local kept = {}
  for _, key in ipairs(stage_order) do
    if items[key] then
      kept[#kept + 1] = key
      items[key].placed = true
    end
  end
  local fresh = {}
  for _, it in pairs(items) do
    if not it.placed then fresh[#fresh + 1] = it end
  end
  -- New items slot by their SAVED stage when reloading a blueprint, else by
  -- the classic default: rings first, axial bottom-up.
  local function saved_of(it)
    local s = nil
    for _, u in ipairs(it.uids) do
      local su = saved_stage[u]
      if su and su > 0 and (not s or su < s) then s = su end
    end
    return s or 1e9
  end
  table.sort(fresh, function(a, b)
    local sa, sb = saved_of(a), saved_of(b)
    if sa ~= sb then return sa < sb end
    if a.ring ~= b.ring then return a.ring end
    return a.y < b.y
  end)
  for _, it in ipairs(fresh) do kept[#kept + 1] = it.key end
  stage_order = kept
  return items
end

-- The number of header lines in the panel (title + subtitle + blank) — the
-- staging-row hit-test skips these. Kept in one place so the panel text and
-- the click math never disagree.
local STAGE_HEADER_LINES = 3

-- Paint the STAGING panel: each separation event is a numbered STAGE (#1
-- fires first), named by what it does. Visible only while the STAGE tool is
-- active (it declutters otherwise). The hovered row is marked ▸ and its
-- decoupler(s) light up in the world (see draw_stage_badges).
local function paint_stages()
  if not stage_node2 then stage_node2 = find("StagePanel") end
  if not stage_node2 then return end
  local items = sync_stage_order()
  local el = stage_node2:getcomponent("UiElement")
  local show = (tool == "stage") and #stage_order > 0
  if not show then
    if el then el.visible = false end
    return
  end
  if el then el.visible = true end
  local lines = { "STAGING  ·  #1 fires first", "drag a row to reorder", "" }
  for i, key in ipairs(stage_order) do
    local it = items[key]
    if it then
      local marker = (stage_drag and stage_drag.key == key) and "▶"
        or (stage_hover == key and "▸") or " "
      if it.ring then
        lines[#lines + 1] = string.format("%s #%d   BOOSTER RING ×%d", marker, i, #it.uids)
      else
        lines[#lines + 1] = string.format("%s #%d   DECOUPLER", marker, i)
      end
    end
  end
  stage_node2.text = table.concat(lines, "\n")
end

-- Draw a numbered badge (a filled disc + the number over it) at each stage's
-- decoupler(s), in the STAGE tool — so the row and the physical part are
-- unmistakably the same one. The hovered row's badges glow.
local function draw_stage_badges()
  if tool ~= "stage" then return end
  local items = sync_stage_order()
  for i, key in ipairs(stage_order) do
    local it = items[key]
    if it then
      local hot = (stage_hover == key) or (stage_drag and stage_drag.key == key)
      local r, g, b = 1.0, 0.75, 0.25
      if hot then r, g, b = 0.3, 1.0, 0.5 end
      for _, uid in ipairs(it.uids) do
        local p = parts[uid]
        if p then
          -- A camera-facing badge disc + a bright ring; the stage number is
          -- read off the panel row of the same color when hovered.
          local up = { x = 0, y = 1, z = 0 }
          draw.disc(p.x, p.y + 0.15, p.z, up.x, up.y, up.z, 0.0, 0.22 + (hot and 0.08 or 0),
            r, g, b, 0.9)
          draw.ring(p.x, p.y + 0.15, p.z, up.x, up.y, up.z, 0.30, r, g, b, 1.0)
          -- little riser so the badge reads above the part
          draw.line(p.x, p.y, p.z, p.x, p.y + 0.15, p.z, r, g, b, 0.8)
        end
      end
    end
  end
end

local function refresh_stats()
  paint_stages()
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
  stats_node.text = string.format("%d parts   %.2f t   $%d%s%s   ⛭ %s%s",
    n, mass, cost, twr_s,
    sym_n > 1 and ("   SYM ×" .. sym_n) or "",
    TOOL_LABEL[tool] or "?", stage_lines())
end

-- ── Undo (robust: ops that can't apply yet re-push and wait) ────────────────
local function push_undo(op)
  undo_stack[#undo_stack + 1] = op
  if #undo_stack > 40 then table.remove(undo_stack, 1) end
end

local function spawn_part(id, x, y, z, yaw, parent, uid, pitch, roll, att, sym)
  local def = REG[id]
  local u = uid or next_uid
  if not uid then next_uid = next_uid + 1 end
  pending_spawns = pending_spawns + 1
  spawn(def.prefab, vec3(x, y, z), function(node)
    node.yaw, node.pitch, node.roll = yaw or 0, pitch or 0, roll or 0
    parts[u] = { id = id, def = def, node = node, x = x, y = y, z = z,
                 yaw = yaw or 0, pitch = pitch or 0, roll = roll or 0,
                 parent = parent, att = att, sym = sym }
    pending_spawns = pending_spawns - 1
    publish_center(); refresh_stats()
  end)
  return u
end

local function remove_part(uid)
  local p = parts[uid]
  if not p then return nil end
  local data = { uid = uid, id = p.id, x = p.x, y = p.y, z = p.z, yaw = p.yaw,
                 pitch = p.pitch, roll = p.roll, parent = p.parent,
                 att = p.att, sym = p.sym }
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
  elseif op.type == "place_group" then
    -- A symmetry ring placed as one op un-places as one op.
    for _, u in ipairs(op.uids) do remove_part(u) end
  elseif op.type == "scrap" then
    for _, d in ipairs(op.parts) do
      spawn_part(d.id, d.x, d.y, d.z, d.yaw, d.parent, d.uid, d.pitch, d.roll,
                 d.att, d.sym)
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
    local dirs
    if p.def.radial then
      -- A mounted radial decoupler's usable face is its disc's OUTWARD +Y
      -- (the inner face is against the hull): ONE node there, and whatever
      -- attaches keeps its own orientation — a vertical booster tank hangs
      -- off the disc exactly like KSP.
      dirs = { { a = Y, r = hh } }
    else
      dirs = {
        { a = X, r = p.def.rx },
        { a = { x = -X.x, y = -X.y, z = -X.z }, r = p.def.rx },
        { a = Z, r = p.def.rz },
        { a = { x = -Z.x, y = -Z.y, z = -Z.z }, r = p.def.rz },
      }
    end
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
-- DEEPLY overlap a placed part? FORGIVING on purpose: parts live in ONE rigid
-- compound and dropped stages don't collide with each other, so touching /
-- clipping is harmless — only a near-total overlap (one part buried inside
-- another) is worth refusing. The old 0.9 gate rejected boosters that merely
-- HUG the hull, forcing the pull-out-then-nudge-back dance. 0.55 = "you'd
-- have to shove it most of the way through" before it's blocked.
local OVERLAP = 0.55
local function overlaps(def, x, y, z, exclude, gy, gp, gr)
  local ax, ay, az = eff_extents(def, gy, gp, gr)
  for uid, p in pairs(parts) do
    if not (exclude and exclude[uid]) then
      local bx, by, bz = part_extents(p)
      if math.abs(x - p.x) < (ax + bx) * OVERLAP
        and math.abs(y - p.y) < (ay + by) * OVERLAP
        and math.abs(z - p.z) < (az + bz) * OVERLAP then
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

-- The part a precise-edit undo op is already open for (one op per streak,
-- shared by the arrow keys AND the drag gizmo).
local nudge_uid = nil

-- Apply a precise edit to `uid`: displacement + rotation deltas, with the
-- ring rules (a symmetry member edits every member, displacement rotated to
-- each angle) and ONE undo op per editing streak. The keybinds and the drag
-- gizmo both land here.
local function apply_edit(uid, ddx, ddy, ddz, dyaw, dpitch, droll)
  local p = parts[uid]
  if not p then return end
  if nudge_uid ~= uid then
    nudge_uid = uid
    local moved = {}
    for u in pairs(edit_set(uid)) do
      local q = parts[u]
      moved[#moved + 1] = { uid = u, x = q.x, y = q.y, z = q.z,
                            yaw = q.yaw, pitch = q.pitch, roll = q.roll, parent = q.parent }
    end
    push_undo({ type = "move", moved = moved })
  end
  local turned = dyaw ~= 0 or dpitch ~= 0 or droll ~= 0
  local hub = p.sym and group_hub(p.sym) or nil
  if hub then
    local a0 = angle_about(hub, p)
    for _, m in ipairs(group_members(p.sym)) do
      local q = parts[m]
      if q then
        local da = angle_about(hub, q) - a0
        local rdx, rdy, rdz = rot_about(hub, ddx, ddy, ddz, da)
        for u in pairs(subtree(m)) do
          local sub = parts[u]
          if sub then set_part_pos(sub, sub.x + rdx, sub.y + rdy, sub.z + rdz) end
        end
        if turned then
          set_part_rot(q, wrap_angle((q.yaw or 0) + dyaw),
                       wrap_angle((q.pitch or 0) + dpitch),
                       wrap_angle((q.roll or 0) + droll))
        end
      end
    end
  else
    if turned then
      set_part_rot(p, wrap_angle((p.yaw or 0) + dyaw),
                   wrap_angle((p.pitch or 0) + dpitch),
                   wrap_angle((p.roll or 0) + droll))
    end
    if ddx ~= 0 or ddy ~= 0 or ddz ~= 0 then
      for u in pairs(subtree(uid)) do
        local q = parts[u]
        if q then set_part_pos(q, q.x + ddx, q.y + ddy, q.z + ddz) end
      end
    end
  end
  publish_center()
  refresh_stats()
end

-- ── Transform gizmos (editor-style, tool-specific) ──────────────────────────
-- ONE gizmo at a time, on the SELECTED part, for the ACTIVE tool: the MOVE
-- tool shows three solid arrows (world X/Y/Z, filled cone heads); the ROTATE
-- tool shows three solid rings (filled bands about those axes). Grab a handle
-- and drag: an arrow moves along its axis, a ring turns about it (SHIFT = fine
-- / 15° snap). Standard R/G/B axis colors so it reads at a glance.
local GIZ_AXES = {
  { x = 1, y = 0, z = 0, r = 0.95, g = 0.30, b = 0.28 }, -- X (red)  → pitch
  { x = 0, y = 1, z = 0, r = 0.40, g = 0.90, b = 0.42 }, -- Y (green)→ yaw
  { x = 0, y = 0, z = 1, r = 0.35, g = 0.55, b = 1.0 },  -- Z (blue) → roll
}
local gizmo_drag = nil -- { uid, kind = "move"|"turn", axis, lmx, lmy, acc }

-- The gizmo's size scales with the selected part so it never swamps a small
-- part or hides inside a big one. Returns the arrow length + ring radius.
local function gizmo_dims(p)
  local ex, ey, ez = part_extents(p)
  local reach = math.max(ex, ey, ez)
  return 1.15 + reach * 0.5, 0.85 + reach * 0.55
end

-- Two unit vectors spanning the plane ⊥ to axis `a` (for the rotate ring).
local function ring_basis(a)
  local ux, uy, uz = a.z, a.x, a.y
  return ux, uy, uz, a.y, a.z, a.x
end

local function draw_gizmo(p, tool)
  local len, rad = gizmo_dims(p)
  if tool == "rotate" then
    for _, a in ipairs(GIZ_AXES) do
      -- A solid band (filled annulus) reads as a real ring, not a thin line.
      draw.disc(p.x, p.y, p.z, a.x, a.y, a.z, rad * 0.9, rad, a.r, a.g, a.b, 0.7)
    end
    -- a small hub so the pivot is obvious
    draw.sphere(p.x, p.y, p.z, 0.09, 0.9, 0.9, 0.95, 1.0)
  else -- move
    for _, a in ipairs(GIZ_AXES) do
      local hx, hy, hz = p.x + a.x * len, p.y + a.y * len, p.z + a.z * len
      -- shaft
      draw.line(p.x, p.y, p.z, hx, hy, hz, a.r, a.g, a.b, 1.0)
      -- solid cone arrowhead (base a bit back from the tip)
      local bl = 0.26
      draw.cone(hx - a.x * bl, hy - a.y * bl, hz - a.z * bl,
        a.x, a.y, a.z, 0.11, bl, a.r, a.g, a.b, 1.0)
    end
    draw.box(p.x, p.y, p.z, 0.09, 0.09, 0.09, 0, 0.95, 0.95, 1.0, 1.0)
  end
  return len, rad
end

-- Which handle is under the cursor for `tool`? → kind, axis index (nil none).
-- Generously sampled: a big rotate ring at a shallow angle projects to a long
-- screen arc, so we walk it densely and use a fat pixel radius — aiming at a
-- ring should GRAB it, never fall through to a deselect.
local GIZ_HIT_PX = 22
local function gizmo_hit(p, tool, len, rad)
  local mx, my = input.mouse()
  local best, best_kind, best_d = nil, nil, GIZ_HIT_PX * GIZ_HIT_PX
  if tool == "rotate" then
    for ai, a in ipairs(GIZ_AXES) do
      local ux, uy, uz, vx2, vy2, vz2 = ring_basis(a)
      for k = 0, 47 do
        local t = k * (2 * math.pi / 48)
        local cx = p.x + (ux * math.cos(t) + vx2 * math.sin(t)) * rad
        local cy2 = p.y + (uy * math.cos(t) + vy2 * math.sin(t)) * rad
        local cz = p.z + (uz * math.cos(t) + vz2 * math.sin(t)) * rad
        local sx, sy, on = screen_of(cx, cy2, cz)
        if on then
          local d = (sx - mx) ^ 2 + (sy - my) ^ 2
          if d < best_d then best, best_kind, best_d = ai, "turn", d end
        end
      end
    end
  else
    for ai, a in ipairs(GIZ_AXES) do
      -- Test many points along the shaft so grabbing anywhere on the arrow
      -- (not just the tip) works — much more forgiving.
      for s = 2, 11 do
        local f = len * s / 11
        local sx, sy, on = screen_of(p.x + a.x * f, p.y + a.y * f, p.z + a.z * f)
        if on then
          local d = (sx - mx) ^ 2 + (sy - my) ^ 2
          if d < best_d then best, best_kind, best_d = ai, "move", d end
        end
      end
    end
  end
  if best then return best_kind, best end
  return nil
end

-- The screen-space bounding radius of the selection's gizmo (center + max
-- reach to any handle tip, in pixels). A click INSIDE this footprint that
-- misses a handle must NOT deselect — the player was aiming at the gizmo.
local function gizmo_footprint_px(p, tool, len, rad)
  local cx, cy, on = screen_of(p.x, p.y, p.z)
  if not on then return nil end
  local reach = (tool == "rotate") and rad or len
  local maxr = 26 -- always swallow a near-center miss
  for _, a in ipairs(GIZ_AXES) do
    for _, s in ipairs({ 1, -1 }) do
      local sx, sy, o2 = screen_of(p.x + a.x * reach * s, p.y + a.y * reach * s,
        p.z + a.z * reach * s)
      if o2 then
        local d = math.sqrt((sx - cx) ^ 2 + (sy - cy) ^ 2)
        if d > maxr then maxr = d end
      end
    end
  end
  return cx, cy, maxr
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
  local grab = edit_set(uid) -- a ring member brings its whole ring
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
  ui("pickup", 0.8) -- lifted off the stack
  hint(HINT_GHOST, 6.0)
end

local function place_ghost(x, y, z, parent, att, sym)
  local placed_uid = nil
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
      placed_uid = ghost.from_uid
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
                 roll = ghost.roll, parent = parent, att = att, sym = sym }
    push_undo({ type = "place", uid = u })
    placed_uid = u
  end
  ghost = nil
  click_cool = 0.15
  ui("place", 0.9) -- the part snaps home
  hint(HINT_IDLE, 0.0); hint_t = 0
  publish_center(); refresh_stats()
  return placed_uid
end

-- ── Symmetry placement ──────────────────────────────────────────────────────
-- The mirrored centers/orientations for placing `gdef` at (x,y,z) around
-- `hub` with ×n symmetry (k = 1 .. n-1; k = 0 is the original placement).
local function symmetry_slots(hub, gdef, gyaw, gpitch, groll, x, y, z, n)
  local out = {}
  for k = 1, n - 1 do
    local a = k * (2 * math.pi / n)
    local vx, vy, vz = rot_about(hub, x - hub.x, y - hub.y, z - hub.z, a)
    local px, py, pz = hub.x + vx, hub.y + vy, hub.z + vz
    local yaw2, pitch2, roll2 = wrap_angle(gyaw + a), gpitch, groll
    if gdef.radial then
      -- Radial decouplers self-orient: disc axis outward at THIS angle.
      local ol = math.sqrt(vx * vx + vy * vy + vz * vz)
      if ol > 1e-4 then
        pitch2, roll2 = outward_orient(vx / ol, vy / ol, vz / ol)
        yaw2 = 0
      end
    elseif gdef.radial_orient then
      -- A fin blade must self-orient from its ACTUAL rotated slot: rot_about
      -- turns the position one way while gyaw+a turns the blade the other, so
      -- only the base fin would point out. Recompute yaw from (slot − hub).
      yaw2, pitch2, roll2 = math.atan2(-vz, vx), 0, 0
    end
    out[#out + 1] = { x = px, y = py, z = pz, yaw = yaw2, pitch = pitch2, roll = roll2 }
  end
  return out
end

-- Mirror a just-placed part from ring member `host` onto every OTHER member
-- of its ring — the stacked-children rule that makes ×N boosters build in
-- N clicks total, not N × parts. The copies form their own (chained) ring.
local function mirror_onto_ring(host_uid, placed_uid)
  local host = parts[host_uid]
  local src = parts[placed_uid]
  if not (host and host.sym and src) then return end
  local hub = group_hub(host.sym)
  if not hub then return end
  local a0 = angle_about(hub, host)
  next_sym = next_sym + 1
  local gid = next_sym
  src.sym = gid
  local uids = { placed_uid }
  for _, m in ipairs(group_members(host.sym)) do
    if m ~= host_uid then
      local da = angle_about(hub, parts[m]) - a0
      local vx, vy, vz = rot_about(hub, src.x - hub.x, src.y - hub.y, src.z - hub.z, da)
      local px, py, pz = hub.x + vx, hub.y + vy, hub.z + vz
      local yaw2, pitch2, roll2 = wrap_angle(src.yaw + da), src.pitch, src.roll
      if src.def.radial and src.att == "radial" then
        local ox, oy, oz = px - parts[m].x, py - parts[m].y, pz - parts[m].z
        local ol = math.sqrt(ox * ox + oy * oy + oz * oz)
        if ol > 1e-4 then
          pitch2, roll2 = outward_orient(ox / ol, oy / ol, oz / ol)
          yaw2 = 0
        end
      elseif src.def.radial_orient and src.att == "radial" then
        -- A fin mirrored onto another flank: re-face its blade outward there.
        yaw2, pitch2, roll2 = math.atan2(-(pz - parts[m].z), px - parts[m].x), 0, 0
      end
      uids[#uids + 1] = spawn_part(src.id, px, py, pz, yaw2, m, nil,
                                   pitch2, roll2, src.att, gid)
    end
  end
  -- One undo op covers the whole mirrored set (replace the single-place op).
  if #undo_stack > 0 and undo_stack[#undo_stack].type == "place"
    and undo_stack[#undo_stack].uid == placed_uid then
    table.remove(undo_stack)
  end
  push_undo({ type = "place_group", uids = uids })
  hint(string.format("mirrored ×%d across the ring", #uids), 2.0)
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
  -- Firing order: each decoupler carries its stage-event index.
  local items = sync_stage_order()
  local stage_of = {}
  for i2, key in ipairs(stage_order) do
    local it = items[key]
    if it then
      for _, u in ipairs(it.uids) do stage_of[u] = i2 end
    end
  end
  local i = 0
  for uid, p in pairs(parts) do
    i = i + 1
    local d = p.def
    bp.parts[i] = {
      uid = uid, id = p.id, prefab = d.prefab, label = d.label,
      x = p.x - centerX, y = p.y - ref_y, z = p.z - centerZ,
      yaw = p.yaw, pitch = p.pitch or 0, roll = p.roll or 0,
      parent = p.parent or 0, att = p.att or "", sym = p.sym or 0,
      stage = stage_of[uid] or 0,
      h = d.h, mass = d.mass, cost = d.cost, kind = d.kind,
      thrust = d.thrust or 0, burn = d.burn or 0, fuel = d.fuel or 0,
      decouple = d.decouple and 1 or 0, legs = d.legs and 1 or 0,
      radial = d.radial and 1 or 0, chute = d.chute and 1 or 0,
      comms = d.comms and 1 or 0, aero = d.aero and 1 or 0,
    }
  end
  save.set("shipyard.blueprint", bp)
  save.flush()
  ui("save", 0.9)
  hint("blueprint saved  ·  " .. i .. " parts", 2.5)
end

local function load_blueprint()
  local bp = save.get("shipyard.blueprint")
  if not bp or not bp.parts then return end
  for uid in pairs(parts) do remove_part(uid) end
  undo_stack = {}
  stage_order, saved_stage = {}, {}
  for _, d in pairs(bp.parts) do
    if (d.stage or 0) > 0 then saved_stage[d.uid] = d.stage end
    spawn_part(d.id, d.x, d.y + params.floor_y, d.z, d.yaw,
               d.parent ~= 0 and d.parent or nil, d.uid, d.pitch, d.roll,
               d.att ~= "" and d.att or nil,
               (d.sym or 0) ~= 0 and d.sym or nil)
    if (d.sym or 0) > next_sym then next_sym = d.sym end
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

-- The STAGING panel (STAGE tool): draw the numbered decoupler badges, then
-- hit-test the panel against its REAL solved rect (node:uiRect — physical px,
-- the mouse's space) and drag rows to reorder the firing sequence. Returns true
-- when the panel owns the mouse this frame. Lives OUTSIDE `update` so that huge
-- function stays under LuaJIT's 60-upvalue-per-function ceiling.
local function handle_staging(clicked, lmb, grab_mode)
  local in_panel = false
  draw_stage_badges()
  stage_hover = nil
  if tool == "stage" and #stage_order > 0 then
    if not stage_node2 then stage_node2 = find("StagePanel") end
    -- NB: `a and f()` truncates f's multi-return to one value — call uiRect
    -- on its own line so all four components survive.
    local rx, ry, rw, rh = 0, 0, 0, 0
    if stage_node2 then rx, ry, rw, rh = stage_node2:uiRect() end
    if rx and rw > 1 then
      local mx, my = input.mouse()
      in_panel = mx >= rx and mx <= rx + rw and my >= ry and my <= ry + rh
      -- Row pitch: total rows (header + events) share the panel's inner
      -- height minus a small pad, so the math tracks the real render.
      local pad = rh * 0.03
      local n_rows = STAGE_HEADER_LINES + #stage_order
      local pitch = (rh - pad * 2) / n_rows
      local function row_of(y)
        local r = math.floor((y - ry - pad) / pitch) - STAGE_HEADER_LINES + 1
        if r < 1 or r > #stage_order then return nil end
        return r
      end
      if in_panel then
        local r = row_of(my)
        if r then stage_hover = stage_order[r] end
      end
      if stage_drag then
        local r = row_of(my)
        if r and r ~= stage_drag.row then
          table.remove(stage_order, stage_drag.row)
          table.insert(stage_order, r, stage_drag.key)
          stage_drag.row = r
          paint_stages()
        end
        if not lmb then
          stage_drag = nil
          click_cool = 0.2
          paint_stages()
          hint("staging order set — it fires #1 first in flight", 2.5)
        end
      elseif in_panel and clicked and not grab_mode then
        local r = row_of(my)
        if r then
          stage_drag = { key = stage_order[r], row = r }
          paint_stages()
        end
      end
      paint_stages() -- refresh the ▸ hover marker
    end
  end
  return stage_drag or in_panel
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

  -- TOOL switch (1 Move · 2 Rotate · 3 Stage · 4 Grab) — never while placing
  -- a part or holding a gizmo. The active tool decides what the SELECTION's
  -- gizmo does; switching tools keeps the selection.
  if not ghost and not gizmo_drag then
    for i, tname in ipairs(TOOLS) do
      if input.pressed(tostring(i)) then
        tool = tname
        ui("tool", 0.7)
        hint("tool: " .. TOOL_LABEL[tool] .. (tool == "grab"
          and " — click a part to pick it up" or ""), 2.0)
        refresh_stats()
      end
    end
  end

  -- SYMMETRY mode: X cycles ×1 ×2 ×3 ×4 ×6 ×8 (SHIFT+X cycles back).
  -- Applies to radial placements; stacking on a ring always auto-mirrors.
  if input.pressed("x") then
    local idx = 1
    for i, n in ipairs(SYM_STEPS) do
      if n == sym_n then idx = i end
    end
    idx = input.key("shift") and ((idx - 2) % #SYM_STEPS + 1) or (idx % #SYM_STEPS + 1)
    sym_n = SYM_STEPS[idx]
    hint(sym_n > 1
      and ("symmetry ×" .. sym_n .. " — a radial placement rings the hull")
      or "symmetry off", 2.0)
    refresh_stats()
  end

  -- ── STAGING panel (STAGE tool): drag rows to reorder the firing sequence ──
  -- Extracted to handle_staging() (below) so this enormous `update` stays under
  -- LuaJIT's 60-upvalue-per-function ceiling. Returns true when the panel owns
  -- the mouse this frame — no picking, placing or scrapping through it.
  if handle_staging(clicked, lmb, grab_mode) then return end

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
      -- face outward (that's the direction its branch will kick away). The
      -- pitch/roll solve assumes yaw 0 — leftover ghost yaw (R presses)
      -- skewed the disc up to 90° off, so it is ZEROED here: the disc is
      -- rotationally symmetric, a yaw spin changes nothing you can see.
      if snap.side == "radial" and ghost.def.radial then
        ghost.yaw = 0
        ghost.roll = math.asin(math.max(-1, math.min(1, -snap.dx)))
        ghost.pitch = math.atan2(snap.dz, snap.dy)
        local gex, gey, gez = eff_extents(ghost.def, ghost.yaw, ghost.pitch, ghost.roll)
        local g = math.abs(snap.dx) * gex + math.abs(snap.dy) * gey + math.abs(snap.dz) * gez
        x = snap.x + snap.dx * g
        y = snap.y + snap.dy * g
        z = snap.z + snap.dz * g
      elseif snap.side == "radial" and ghost.def.radial_orient then
        -- A fin: yaw its blade to face outward (horizontal), upright, re-seat.
        ghost.yaw = math.atan2(-snap.dz, snap.dx)
        ghost.pitch, ghost.roll = 0, 0
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
    -- Symmetry ring this placement would make: a fresh ×N ring on a plain
    -- radial snap (never on a ring member — those AUTO-mirror instead).
    local sym_slots = nil
    if snap then
      local ex2 = { [snap.uid] = true }
      if exclude then for u in pairs(exclude) do ex2[u] = true end end
      -- A RADIAL mount hugs the hull by design — never overlap-gate it (that
      -- was the "pull it out then push it back" jank). Stack snaps still
      -- reject only a DEEP overlap.
      local blocked = snap.side ~= "radial"
        and overlaps(ghost.def, x, y, z, ex2, ghost.yaw, ghost.pitch, ghost.roll)
      if blocked then
        can_place, why = false, "that spot is buried inside another part"
        draw.sphere(x, y, z, 0.35, 1.0, 0.25, 0.2, 1.0)
      else
        can_place = true
        draw.line(x, y, z, snap.x, snap.y, snap.z, 0.3, 1.0, 0.4, 1.0)
        draw.ring(snap.x, snap.y, snap.z, 0, 1, 0, 0.4, 0.3, 1.0, 0.4, 1.0)
      end
      if can_place and snap.side == "radial" and sym_n > 1 and not ghost.carried then
        local host = parts[snap.uid]
        if host and not host.sym then
          sym_slots = symmetry_slots(host, ghost.def, ghost.yaw, ghost.pitch,
                                     ghost.roll, x, y, z, sym_n)
          for _, s in ipairs(sym_slots) do
            -- Radial ring copies hug the hull too — preview only, no gate.
            local bx3, by3, bz3 = eff_extents(ghost.def, s.yaw, s.pitch, s.roll)
            draw.box(s.x, s.y, s.z, bx3, by3, bz3, s.yaw, 1.0, 0.75, 0.3, 0.55)
          end
        end
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
        ui("scrap", 0.8)
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
        local host_uid = snap and snap.uid or nil
        local host = host_uid and parts[host_uid]
        -- Placing ON a ring member auto-mirrors across its whole ring;
        -- sym_slots means a fresh ×N ring around a plain host.
        local on_ring = (host and host.sym and not ghost.carried) or false
        local gdata = sym_slots and { id = ghost.id } or nil
        local gid = nil
        if sym_slots then
          next_sym = next_sym + 1
          gid = next_sym
        end
        local placed = place_ghost(x, y, z, host_uid, snap and snap.side or nil, gid)
        selected_uid = placed -- the freshly placed part becomes the selection
        if sym_slots and placed then
          local uids = { placed }
          for _, s in ipairs(sym_slots) do
            uids[#uids + 1] = spawn_part(gdata.id, s.x, s.y, s.z, s.yaw,
                                         host_uid, nil, s.pitch, s.roll, "radial", gid)
          end
          -- The ring un-places as ONE undo op.
          if #undo_stack > 0 and undo_stack[#undo_stack].type == "place"
            and undo_stack[#undo_stack].uid == placed then
            table.remove(undo_stack)
          end
          push_undo({ type = "place_group", uids = uids })
          hint(string.format("×%d ring placed — CTRL+Z removes it all", #uids), 2.5)
        elseif on_ring and placed then
          mirror_onto_ring(host_uid, placed)
        end
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

  -- ── Gizmo drag in progress: it owns the mouse until release ──
  if gizmo_drag then
    local p = parts[gizmo_drag.uid]
    if not lmb or not p then
      gizmo_drag = nil
      click_cool = 0.12
    else
      local mx, my = input.mouse()
      local dmx, dmy = mx - gizmo_drag.lmx, my - gizmo_drag.lmy
      gizmo_drag.lmx, gizmo_drag.lmy = mx, my
      local a = GIZ_AXES[gizmo_drag.axis]
      draw_gizmo(p, tool)
      outline(p, 0.55, 0.85, 1.0, 1.0)
      if gizmo_drag.kind == "move" then
        -- Mouse motion projected onto the arrow's screen direction.
        local sx0, sy0 = screen_of(p.x, p.y, p.z)
        local sx1, sy1 = screen_of(p.x + a.x, p.y + a.y, p.z + a.z)
        local vx2, vy2 = sx1 - sx0, sy1 - sy0
        local l2 = vx2 * vx2 + vy2 * vy2
        if l2 > 1e-3 and (dmx ~= 0 or dmy ~= 0) then
          local t2 = (dmx * vx2 + dmy * vy2) / l2
          if input.key("shift") then t2 = t2 * 0.25 end -- fine control
          apply_edit(gizmo_drag.uid, a.x * t2, a.y * t2, a.z * t2, 0, 0, 0)
        end
        hint("MOVE — drag the arrow  ·  SHIFT fine  ·  release to set", 1.0)
      else
        -- Ring turn: horizontal+vertical drag turns about the ring's axis;
        -- SHIFT snaps the accumulated angle to 15° notches.
        local dang = (dmx - dmy) * 0.01
        if dang ~= 0 then
          if input.key("shift") then
            gizmo_drag.acc = (gizmo_drag.acc or 0) + dang
            local notch = math.pi / 12
            local steps = math.floor(gizmo_drag.acc / notch + 0.5)
            dang = steps * notch - (gizmo_drag.snapped or 0)
            gizmo_drag.snapped = steps * notch
          end
          if dang ~= 0 then
            local dy2 = (gizmo_drag.axis == 2) and dang or 0
            local dp2 = (gizmo_drag.axis == 1) and dang or 0
            local dr2 = (gizmo_drag.axis == 3) and dang or 0
            apply_edit(gizmo_drag.uid, 0, 0, 0, dy2, dp2, dr2)
          end
        end
        hint("ROTATE — drag the ring  ·  SHIFT snaps 15°  ·  release to set", 1.0)
      end
    end
    return
  end

  -- ── Hover preview + SELECT + tool action ──
  -- Clicking a part SELECTS it (persists); hovering only previews what a
  -- click would pick. The selection's gizmo follows the active TOOL — never
  -- the cursor — so nudging one part can't grab another.
  hover_uid = (not cam_busy) and part_under_cursor(70) or nil
  local sel = selected_uid and parts[selected_uid]
  if not sel then selected_uid = nil end

  -- The selected part: bright outline + (in Move/Rotate) its tool gizmo.
  local giz_len, giz_rad = nil, nil
  if sel then
    outline(sel, 0.4, 0.95, 0.7, 1.0)
    for u in pairs(edit_set(selected_uid)) do
      if u ~= selected_uid and parts[u] then outline(parts[u], 0.4, 0.95, 0.7, 0.3) end
    end
    if tool == "move" or tool == "rotate" then
      giz_len, giz_rad = draw_gizmo(sel, tool)
      if clicked and not cam_busy then
        local kind, ai = gizmo_hit(sel, tool, giz_len, giz_rad)
        if kind then
          local mx, my = input.mouse()
          gizmo_drag = { uid = selected_uid, kind = kind, axis = ai, lmx = mx, lmy = my }
          return -- a gizmo grab never re-selects
        end
      end
    end
  end

  -- Hover preview: a soft outline of what a click would select.
  if hover_uid and hover_uid ~= selected_uid then
    outline(parts[hover_uid], 0.8, 0.88, 1.0, 0.45)
  end

  -- Click: SELECT the hovered part, or (GRAB tool) pick it up to re-attach.
  -- Clicking empty space deselects — EXCEPT a click that lands on the active
  -- gizmo's footprint (a missed handle grab), which keeps the selection so
  -- fiddling with the Move/Rotate gizmo never drops what you're editing.
  if clicked and not cam_busy then
    if hover_uid then
      if tool == "grab" then
        pickup(hover_uid) -- carry flow → re-place at a new attach node
        selected_uid = nil
      else
        selected_uid = hover_uid
      end
      return
    end
    if sel and giz_len then
      local gx, gy, gr = gizmo_footprint_px(sel, tool, giz_len, giz_rad)
      if gx then
        local mx, my = input.mouse()
        if (mx - gx) ^ 2 + (my - gy) ^ 2 <= gr * gr then return end -- keep selection
      end
    end
    selected_uid = nil -- clicked the empty canvas, clear of the gizmo
    return
  end

  -- ── Selection editing (acts on the SELECTION; a fallback to the gizmo) ──
  if sel then
    local p = sel
    -- Arrow keys nudge (↑/↓ vertical, ←/→ screen-horizontal, ALT+←/→ depth,
    -- SHIFT fine); R/T/Y rotate. All route through apply_edit on the
    -- selection (ring-symmetric, one undo op per streak).
    local step = input.key("shift") and params.nudge * 0.25 or params.nudge
    local ddx, ddy, ddz = 0, 0, 0
    if input.pressed("up") then ddy = step end
    if input.pressed("down") then ddy = -step end
    local h = (input.pressed("right") and 1 or 0) - (input.pressed("left") and 1 or 0)
    if h ~= 0 then
      local mx, my = input.mouse()
      local _, _, _, d1x, _, d1z = camera.screenToRay(mx, my)
      local _, _, _, d2x, _, d2z = camera.screenToRay(mx + 40, my)
      local rxx, rzz = d2x - d1x, d2z - d1z
      if input.key("alt") then rxx, rzz = -rzz, rxx end
      if math.abs(rxx) >= math.abs(rzz) then
        ddx = (rxx >= 0 and 1 or -1) * h * step
      else
        ddz = (rzz >= 0 and 1 or -1) * h * step
      end
    end
    local rot = { yaw = p.yaw or 0, pitch = p.pitch or 0, roll = p.roll or 0 }
    local turned = rot_input(rot)
    if ddx ~= 0 or ddy ~= 0 or ddz ~= 0 or turned then
      apply_edit(selected_uid, ddx, ddy, ddz,
        turned and (rot.yaw - (p.yaw or 0)) or 0,
        turned and (rot.pitch - (p.pitch or 0)) or 0,
        turned and (rot.roll - (p.roll or 0)) or 0)
    end
    -- DEL scraps the selection + its stack (a ring member takes the ring).
    if input.pressed("delete") or input.pressed("del") then
      local grab = edit_set(selected_uid)
      local datas = {}
      for u in pairs(grab) do
        local d = remove_part(u)
        if d then datas[#datas + 1] = d end
      end
      push_undo({ type = "scrap", parts = datas })
      ui("scrap", 0.8)
      hint("scrapped " .. #datas .. " part(s) — CTRL+Z to undo", 2.5)
      selected_uid = nil
    end
  else
    nudge_uid = nil -- nothing selected: next edit opens a fresh undo op
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
  ui("launch", 1.0)
  save.set("shipyard.launch", 1)
  save.set("shipyard.pilot", 1) -- you launch IN the pod, not beside it
  save.flush()
  scene.load("system")
end
