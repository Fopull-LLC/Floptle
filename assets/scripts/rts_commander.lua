-- RTS mouse command: select units, right-click to send them.
--
-- SETUP: attach to any node — the camera running rts_camera.lua is the natural
-- home. Give every commandable node the rts_unit.lua script. That's the whole
-- setup; this script finds them by script, not by name or tag.
--
--   Left click     select the unit under the cursor (empty ground clears)
--   Left drag      box-select everything inside the box
--   Sprint (Shift) add to the selection instead of replacing it
--   Right click    send the selection there, spread into a loose formation
--
-- The box is drawn on the GROUND rather than as a screen rectangle: it reads
-- correctly at any camera angle, needs no UI nodes, and picks exactly what it
-- outlines. `draw.*` is immediate mode, so it lives exactly as long as the drag.
--
-- Everything here goes through rts_unit's small API (`moveTo`, `stop`,
-- `selected`), so swapping in your own unit script means keeping three names.

defaults = {
  --@header World
  -- The ground height clicks are resolved against. A click ray is intersected
  -- with this plane, so it works with no colliders in the scene at all; with
  -- terrain or blockout under the cursor, the surface hit wins.
  --@range -100 100 --@units m
  ground_y = 0,
  --@range 10 10000 --@units m
  pick_range = 4000,
  --@header Orders
  -- Spacing of the arrival grid, so a group doesn't pile onto one point.
  --@range 0 20 --@units m
  formation_spacing = 2.5,
  -- Seconds the click marker stays on the ground.
  --@range 0 5 --@units s
  marker_time = 0.7,
  --@header Selection
  -- Pixels of travel before a click becomes a box drag.
  --@range 2 40 --@units px
  drag_threshold = 6,
}

-- Public: how many units are selected (a HUD can read this).
count = 0

local drag = nil -- { sx, sy, wx, wz } while the left button is down
local marker = nil -- { x, z, t } fading order marker

-- Where a screen pixel meets the world: the first thing the ray hits, or the
-- ground plane if it hits nothing (an empty scene still commands correctly).
local function pick(mx, my)
  local ox, oy, oz, dx, dy, dz = camera.screenToRay(mx, my)
  if not ox then return nil end
  local hit = raycast(ox, oy, oz, dx, dy, dz, params.pick_range)
  if hit then return hit.x, hit.y, hit.z, hit.node end
  if dy >= -1e-6 then return nil end -- ray points up/level: never meets the plane
  local t = (params.ground_y - oy) / dy
  return ox + dx * t, params.ground_y, oz + dz * t, nil
end

local function units()
  return findScripts("rts_unit")
end

local function clear_selection()
  for _, u in ipairs(units()) do u.selected = false end
end

function update(node, dt)
  local mx, my = input.mouse()

  -- ---- selection ---------------------------------------------------------
  if input.clicked(0) then
    local x, _, z = pick(mx, my)
    drag = { sx = mx, sy = my, wx = x or 0, wz = z or 0, world = x ~= nil }
  end
  if drag and input.button(0) then
    -- Draw the box being dragged, on the ground, corner to corner.
    local x, _, z = pick(mx, my)
    if x and drag.world then
      local x0, z0, x1, z1 = drag.wx, drag.wz, x, z
      local y = params.ground_y + 0.05
      draw.line(x0, y, z0, x1, y, z0, 0.4, 1.0, 0.6)
      draw.line(x1, y, z0, x1, y, z1, 0.4, 1.0, 0.6)
      draw.line(x1, y, z1, x0, y, z1, 0.4, 1.0, 0.6)
      draw.line(x0, y, z1, x0, y, z0, 0.4, 1.0, 0.6)
    end
  end
  if drag and not input.button(0) then
    local add = input.action("Sprint") -- the shipped Shift binding; rebindable
    local moved = math.abs(mx - drag.sx) + math.abs(my - drag.sy)
    if not add then clear_selection() end
    if moved > params.drag_threshold and drag.world then
      -- Box: everything whose position falls inside the dragged ground rect.
      local x, _, z = pick(mx, my)
      if x then
        local lo_x, hi_x = math.min(drag.wx, x), math.max(drag.wx, x)
        local lo_z, hi_z = math.min(drag.wz, z), math.max(drag.wz, z)
        for _, u in ipairs(units()) do
          local n = u.node
          if n.x >= lo_x and n.x <= hi_x and n.z >= lo_z and n.z <= hi_z then
            u.selected = true
          end
        end
      end
    else
      -- Click: whatever unit is under the cursor (nothing = a deselect).
      local _, _, _, hit_node = pick(mx, my)
      local u = hit_node and hit_node:getscript("rts_unit")
      if u then u.selected = not (add and u.selected) end
    end
    drag = nil
  end

  -- ---- orders ------------------------------------------------------------
  if input.clicked(1) then
    local x, y, z = pick(mx, my)
    if x then
      local sel = {}
      for _, u in ipairs(units()) do
        if u.selected then sel[#sel + 1] = u end
      end
      -- A loose square grid centred on the click, so a group arrives spread
      -- out instead of fighting over one square metre.
      local side = math.max(1, math.ceil(math.sqrt(#sel)))
      local step = params.formation_spacing
      for i, u in ipairs(sel) do
        local col = (i - 1) % side - (side - 1) * 0.5
        local row = math.floor((i - 1) / side) - (side - 1) * 0.5
        u.moveTo(x + col * step, y, z + row * step)
      end
      if #sel > 0 then marker = { x = x, z = z, t = params.marker_time } end
    end
  end

  -- ---- feedback ----------------------------------------------------------
  count = 0
  for _, u in ipairs(units()) do
    if u.selected then count = count + 1 end
  end
  if marker then
    marker.t = marker.t - dt
    if marker.t <= 0 then
      marker = nil
    else
      local r = 0.6 + (params.marker_time - marker.t) * 2.0 -- expanding pulse
      local y = params.ground_y + 0.06
      local px, pz = marker.x + r, marker.z
      for i = 1, 16 do
        local a = (i / 16) * math.pi * 2
        local cx, cz = marker.x + math.cos(a) * r, marker.z + math.sin(a) * r
        draw.line(px, y, pz, cx, y, cz, 1.0, 0.85, 0.3)
        px, pz = cx, cz
      end
    end
  end
end
