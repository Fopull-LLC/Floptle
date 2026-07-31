-- Isometric RTS camera — the strategy-game view, ready to build on.
--
-- SETUP: make a Camera node, mark it Active, attach this script. Nothing else:
-- the script owns the camera's whole transform, so wherever you leave it in the
-- editor, Play snaps it to a clean isometric framing of the ground beneath it.
--
--   Pan      WASD or the left stick (Move) — along the ground, screen-relative
--   Edge pan push the mouse to a screen edge
--   Zoom     the wheel or the d-pad (Zoom)
--   Rotate   Q / E or the bumpers (Turn) — swings the view about the focus point
--   Fast     hold Sprint (Shift / L3) to pan quicker
--
-- Every control is a NAMED ACTION from Project Settings → Input, so this works
-- on a keyboard, on a gamepad, or on both, and all of it is rebindable without
-- touching the code.
--
-- The camera looks at a FOCUS POINT on the ground plane and orbits it at a
-- fixed pitch. Panning moves the focus, zooming changes the distance to it,
-- rotating swings the yaw around it — which is what makes a click on the ground
-- land where the player expects however far out they are zoomed.
--
-- Pairs with rts_unit.lua (units that take move orders) and rts_commander.lua
-- (select with the mouse, right-click to order). Other scripts can drive it:
--
--   local cam = findScript("rts_camera")
--   cam.focusOn(x, z)          -- jump the view to a place (an alert, a base)
--   cam.follow = someUnitNode  -- or have it trail a node until the player pans

defaults = {
  --@header Framing
  -- How steep the view is. 90 is straight down; the classic RTS look is 45–60.
  --@slider 15 89 --@step 1 --@units degrees
  pitch = 55,
  -- Which way is "up the screen". 45 gives the diagonal isometric look.
  --@slider 0 360 --@step 5 --@units degrees
  yaw = 45,
  -- The ground height the camera frames (your terrain's playfield level).
  --@range -100 100 --@units m
  ground_y = 0,
  --@header Panning
  --@range 1 200 --@units m/s
  pan_speed = 28,
  -- Shift multiplier.
  --@range 1 6
  fast_pan = 2.5,
  -- Push the cursor to a screen edge to pan that way. Off for windowed
  -- development, on for a full-screen build — a param, so it can be a setting.
  edge_pan = true,
  -- How close to the edge (in pixels) starts panning.
  --@range 2 80 --@units px
  edge_margin = 14,
  --@header Zoom
  --@range 2 200 --@units m
  distance = 40,
  --@range 2 200 --@units m
  min_distance = 10,
  --@range 2 400 --@units m
  max_distance = 120,
  --@range 0 40
  zoom_speed = 4,
  --@header Rotate
  --@range 0 360 --@units degrees/s
  rotate_speed = 90,
  --@header Feel & limits
  -- Follow-through on pan/zoom/rotate. 0 = instant, higher = snappier glide.
  --@range 0 30
  smoothing = 12,
  -- Half-size of the square the focus may roam, around the world origin.
  -- 0 = unlimited (an endless map, or your own clamp in another script).
  --@range 0 5000 --@units m
  bounds = 0,
}

-- Where the camera is looking (ground plane), and the live orbit — public, so
-- a minimap or a cutscene script can read or write them.
focus_x, focus_z = 0.0, 0.0
yaw_now, dist_now = 0.0, 40.0
-- Set to a node handle to trail it; any manual pan input drops the follow.
follow = nil

-- Targets the live values ease toward (that's all `smoothing` is).
local want_x, want_z, want_yaw, want_dist = 0.0, 0.0, 0.0, 40.0

local function clamp(v, lo, hi)
  if v < lo then return lo end
  if v > hi then return hi end
  return v
end

-- Frame-rate-independent exponential ease: the same feel at 30 and 240 fps.
local function ease(a, b, rate, dt)
  if rate <= 0 then return b end
  return a + (b - a) * (1.0 - math.exp(-rate * dt))
end

-- Point the view at a place on the ground (and stop following anything).
function focusOn(x, z)
  want_x, want_z, focus_x, focus_z = x, z, x, z
  follow = nil
end

function start(node)
  want_yaw, yaw_now = math.rad(params.yaw), math.rad(params.yaw)
  want_dist = clamp(params.distance, params.min_distance, params.max_distance)
  dist_now = want_dist
  -- Frame whatever is under the camera you placed, so Play doesn't teleport the
  -- view somewhere unrelated to the shot you set up in the editor.
  focusOn(node.x, node.z)
end

function update(node, dt)
  -- ---- input ------------------------------------------------------------
  local sx, sy = input.axis2("Move")

  -- Edge pan: only while the cursor is actually over the game view, so a mouse
  -- resting on your other monitor (or on a HUD panel off the edge) can't shove
  -- the map sideways forever.
  if params.edge_pan then
    local w, h = camera.screenSize()
    local mx, my = input.mouse()
    local m = params.edge_margin
    if mx >= 0 and my >= 0 and mx <= w and my <= h then
      if mx < m then sx = -1 elseif mx > w - m then sx = 1 end
      if my < m then sy = 1 elseif my > h - m then sy = -1 end
    end
  end

  -- ---- zoom & rotate ----------------------------------------------------
  local scroll = input.axis1("Zoom")
  if scroll ~= 0 then
    -- Zoom in proportion to how far out you are: one notch reads the same at
    -- every distance instead of crawling up close and lurching far away.
    want_dist = clamp(
      want_dist - scroll * params.zoom_speed * (want_dist * 0.05 + 1.0),
      params.min_distance,
      params.max_distance
    )
  end
  local turn = input.axis1("Turn")
  if turn ~= 0 then
    want_yaw = want_yaw + turn * math.rad(params.rotate_speed) * dt
  end

  -- ---- pan --------------------------------------------------------------
  -- Screen-relative: "up" is up the screen whatever the yaw, which is what
  -- makes a rotated view still steer the way the player is looking.
  if sx ~= 0 or sy ~= 0 then
    follow = nil -- taking the wheel drops any follow
    local speed = params.pan_speed * (want_dist / 40.0) -- zoomed out = broader strokes
    if input.action("Sprint") then speed = speed * params.fast_pan end
    local c, s = math.cos(yaw_now), math.sin(yaw_now)
    -- Ground forward for this yaw (engine forward is −Z), and its right.
    want_x = want_x + (-s * sy + c * sx) * speed * dt
    want_z = want_z + (-c * sy - s * sx) * speed * dt
  elseif follow then
    want_x, want_z = follow.x, follow.z
  end
  if params.bounds > 0 then
    want_x = clamp(want_x, -params.bounds, params.bounds)
    want_z = clamp(want_z, -params.bounds, params.bounds)
  end

  -- ---- ease, then place the camera --------------------------------------
  local r = params.smoothing
  focus_x = ease(focus_x, want_x, r, dt)
  focus_z = ease(focus_z, want_z, r, dt)
  yaw_now = ease(yaw_now, want_yaw, r, dt)
  dist_now = ease(dist_now, want_dist, r, dt)

  local pitch = math.rad(clamp(params.pitch, 15, 89))
  local cp, sp = math.cos(pitch), math.sin(pitch)
  -- Look direction for (yaw, −pitch); the camera sits back along it.
  local fx = -math.sin(yaw_now) * cp
  local fy = -sp
  local fz = -math.cos(yaw_now) * cp
  node.x = focus_x - fx * dist_now
  node.y = params.ground_y - fy * dist_now
  node.z = focus_z - fz * dist_now
  node.yaw = yaw_now
  node.pitch = -pitch
  node.roll = 0
end
