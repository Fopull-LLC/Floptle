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
-- The selection box is a SCREEN rectangle — `draw.rect` / `draw.rectOutline`
-- draw in the same pixels the mouse reports, so the box is literally the two
-- corners you dragged between, and a unit is inside it when its position lands
-- inside it on screen (`camera.worldToScreen`). No ground plane, no camera
-- angle, no projection to fight: this is how every RTS does marquee selection.
--
-- Everything here goes through rts_unit's small API (`moveTo`, `stop`,
-- `selected`), so swapping in your own unit script means keeping three names.

defaults = {
  --@header World
  -- The ground height clicks fall back to. A click ray is intersected with this
  -- plane when it hits nothing solid, so this works in a scene with no
  -- colliders at all; with terrain or blockout under the cursor, the real
  -- surface hit wins.
  --@range -100 100 --@units m
  ground_y = 0,
  --@range 10 10000 --@units m
  pick_range = 4000,
  --@header Orders
  -- Spacing of the arrival grid, so a group doesn't pile onto one point.
  --@range 0 20 --@units m
  formation_spacing = 2.5,
  -- A particle one-shot fired where you ordered the units: the key of any
  -- effect asset (Assets ⏵ ❋ Particles), or blank for none. The drawn ring
  -- below happens either way, so a missing effect is never a dead click.
  marker_effect = "vfx/MoveMarker",
  -- Seconds the drawn click ring stays on the ground.
  --@range 0 5 --@units s
  marker_time = 0.7,
  --@header Selection
  -- Pixels of travel before a click becomes a box drag.
  --@range 2 40 --@units px
  drag_threshold = 6,
}

-- Public: how many units are selected (a HUD can read this).
count = 0

local drag = nil -- { x, y } screen pixel the press started at
local marker = nil -- { x, y, z, t } fading order marker

-- Where a screen pixel meets the world: the first thing the ray hits, else the
-- ground plane. Returns x, y, z, node (node = nil when it hit the plane).
local function pick(mx, my)
  local ox, oy, oz, dx, dy, dz = camera.screenToRay(mx, my)
  if not ox then return nil end
  local hit = raycast(ox, oy, oz, dx, dy, dz, params.pick_range)
  if hit then return hit.x, hit.y, hit.z, hit.node end
  if dy >= -1e-6 then return nil end -- ray points up or level: never meets the plane
  local t = (params.ground_y - oy) / dy
  return ox + dx * t, params.ground_y, oz + dz * t, nil
end

local function units()
  return findScripts("rts_unit")
end

function update(node, dt)
  local mx, my = input.mouse()

  -- ---- selection ---------------------------------------------------------
  if input.clicked(0) then
    drag = { x = mx, y = my }
  end
  if drag and input.button(0) then
    -- The live marquee: a translucent fill plus a bright outline, in pixels.
    local x, y = math.min(drag.x, mx), math.min(drag.y, my)
    local w, h = math.abs(mx - drag.x), math.abs(my - drag.y)
    if w + h > params.drag_threshold then
      draw.rect(x, y, w, h, 0.35, 1.0, 0.55, 0.12)
      draw.rectOutline(x, y, w, h, 0.45, 1.0, 0.6, 0.9, 1.5)
    end
  end
  if drag and not input.button(0) then
    local add = input.action("Sprint") -- the shipped Shift binding; rebindable
    local moved = math.abs(mx - drag.x) + math.abs(my - drag.y)
    if not add then
      for _, u in ipairs(units()) do u.selected = false end
    end
    if moved > params.drag_threshold then
      -- Box: every unit whose position projects inside the rectangle.
      local lo_x, hi_x = math.min(drag.x, mx), math.max(drag.x, mx)
      local lo_y, hi_y = math.min(drag.y, my), math.max(drag.y, my)
      for _, u in ipairs(units()) do
        local n = u.node
        local sx, sy, _, on = camera.worldToScreen(n.worldX, n.worldY, n.worldZ)
        if on and sx >= lo_x and sx <= hi_x and sy >= lo_y and sy <= hi_y then
          u.selected = true
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
      -- A loose square grid centred on the click, so a group arrives spread out
      -- instead of fighting over one square metre.
      local side = math.max(1, math.ceil(math.sqrt(#sel)))
      local step = params.formation_spacing
      for i, u in ipairs(sel) do
        local col = (i - 1) % side - (side - 1) * 0.5
        local row = math.floor((i - 1) / side) - (side - 1) * 0.5
        u.moveTo(x + col * step, y, z + row * step)
      end
      if #sel > 0 then
        marker = { x = x, y = y, z = z, t = params.marker_time }
        if params.marker_effect ~= "" then
          spawnEffect(params.marker_effect, x, y + 0.1, z)
        end
      end
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
      -- An expanding ring on the ground, under the particle puff.
      local r = 0.6 + (params.marker_time - marker.t) * 2.0
      local y = marker.y + 0.06
      local px, pz = marker.x + r, marker.z
      for i = 1, 16 do
        local a = (i / 16) * math.pi * 2
        local cx, cz = marker.x + math.cos(a) * r, marker.z + math.sin(a) * r
        draw.line(px, y, pz, cx, y, cz, 1.0, 0.85, 0.3, marker.t / params.marker_time)
        px, pz = cx, cz
      end
    end
  end
end
