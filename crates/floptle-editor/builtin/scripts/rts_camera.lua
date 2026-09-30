-- Isometric RTS camera — the strategy-game view, ready to build on.
--
-- SETUP: make a Camera node, mark it Active, attach this script. Nothing else:
-- the script owns the camera's whole transform, so wherever you leave it in the
-- editor, Play snaps it to a clean isometric framing of the ground beneath it.
--
--   Pan      WASD or the left stick (Move) — along the ground, screen-relative
--   Edge pan push the mouse to a screen edge
--   Zoom     the wheel, ↑ / ↓, or the d-pad (Zoom)
--   Rotate   Q / E, ← / →, or the bumpers (Turn) — swings the view about the
--            focus point
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
  -- Seconds between steps while a zoom key (or the d-pad) is HELD. A wheel
  -- notch is one step and never waits for this.
  --@range 0.02 1 --@units s
  zoom_repeat = 0.07,
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
-- Zoom auto-repeat timer (see the zoom block in `update`).
local zoom_cool = 0.0

-- Point the view at a place on the ground (and stop following anything).
function focusOn(x, z)
  want_x, want_z, focus_x, focus_z = x, z, x, z
  follow = nil
end

function start(node)
  want_yaw, yaw_now = math.rad(params.yaw), math.rad(params.yaw)
  want_dist = math.clamp(params.distance, params.min_distance, params.max_distance)
  dist_now = want_dist
  -- Frame whatever is under the camera you placed, so Play doesn't teleport the
  -- view somewhere unrelated to the shot you set up in the editor.
  focusOn(node.x, node.z)
end

function update(node, dt)
  -- ---- input ------------------------------------------------------------
  local sx, sy = input.axis2("Move")

  -- Edge pan, measured against the game view's own RECTANGLE — `screenRect`
  -- gives its top-left as well as its size, in the same pixels `input.mouse()`
  -- reports. Size alone is not enough: in the editor the view is a docked panel,
  -- so the cursor's x carries the offset of everything to its left, reads as
  -- "past the right edge" from the moment you open it, and the camera slides
  -- away forever. Outside the rect, nothing pans at all.
  if params.edge_pan then
    local vx, vy, w, h = camera.screenRect()
    local mx, my = input.mouse()
    local m = params.edge_margin
    if mx >= vx and my >= vy and mx <= vx + w and my <= vy + h then
      if mx < vx + m then sx = -1 elseif mx > vx + w - m then sx = 1 end
      if my < vy + m then sy = 1 elseif my > vy + h - m then sy = -1 end
    end
  end

  -- ---- zoom & rotate ----------------------------------------------------
  -- One axis carries both a WHEEL (a spike for a single frame) and HELD keys /
  -- a d-pad (a steady 1 for as long as you hold). Treated as a rate, a wheel
  -- notch would be invisible; treated as a step, a held key would fire sixty
  -- times a second. So: one step per press, then a repeat rate while it stays
  -- down — the same rule a keyboard's own auto-repeat uses.
  local zoom = input.axis1("Zoom")
  zoom_cool = zoom_cool - dt
  if zoom ~= 0 and zoom_cool <= 0 then
    -- Zoom in proportion to how far out you are: one notch reads the same at
    -- every distance instead of crawling up close and lurching far away.
    want_dist = math.clamp(
      want_dist - zoom * params.zoom_speed * (want_dist * 0.05 + 1.0),
      params.min_distance,
      params.max_distance
    )
    zoom_cool = params.zoom_repeat
  elseif zoom == 0 then
    zoom_cool = 0 -- released: the next notch is immediate
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
    -- Ground forward for this yaw (engine forward is −Z), and its right.
    local fwd = dirFromYaw(yaw_now)
    local right = fwd:cross(vec3(0, 1, 0))
    local step = (fwd * sy + right * sx) * (speed * dt)
    want_x, want_z = want_x + step.x, want_z + step.z
  elseif follow then
    want_x, want_z = follow.x, follow.z
  end
  if params.bounds > 0 then
    want_x = math.clamp(want_x, -params.bounds, params.bounds)
    want_z = math.clamp(want_z, -params.bounds, params.bounds)
  end

  -- ---- ease, then place the camera --------------------------------------
  -- `ease` is the engine's frame-rate-independent exponential ease: the same
  -- feel at 30 fps and at 240 (that is all `smoothing` is).
  local r = params.smoothing
  focus_x = ease(focus_x, want_x, r, dt)
  focus_z = ease(focus_z, want_z, r, dt)
  yaw_now = ease(yaw_now, want_yaw, r, dt)
  dist_now = ease(dist_now, want_dist, r, dt)

  -- Look direction for (yaw, −pitch); the camera sits back along it.
  local pitch = math.rad(math.clamp(params.pitch, 15, 89))
  local look = dirFromYaw(yaw_now, -pitch)
  node.pos = vec3(focus_x, params.ground_y, focus_z) - look * dist_now
  node.yaw, node.pitch, node.roll = yaw_now, -pitch, 0
end
