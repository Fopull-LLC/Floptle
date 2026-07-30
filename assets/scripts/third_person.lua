-- Third-person character controller (pairs with third_person_camera.lua).
--
-- SETUP:
--   1. Make your player body: a node with a *Capsule* Rigidbody (lock its
--      rotation so it stays upright), and attach this script.
--   2. Parent your character model to it as a child named "Model" (a rigged
--      .glb animates automatically — give it an Animation Controller with
--      Idle / Walk / Run / Jump states for full control).
--   3. Make a Camera node, mark it Active, attach third_person_camera.lua.
--
--   Move       WASD or the left stick — relative to the camera
--   Jump       Space or the pad's South button (when grounded)
--   Sprint     Shift or L3 (hold) to run — OR double-tap a direction, which
--              works on a stick flick just as well as on a key
--   ShiftLock  Shift / R3 toggles shift lock (the camera script owns the mode;
--              while locked — or in first person — the character faces the
--              camera's yaw instead of its travel)
--
-- Every control is a NAMED ACTION from Project Settings → Input, so this works
-- on a keyboard, on a gamepad, or on both at once, and any of it can be rebound
-- without touching the code.
--
-- Movement is camera-relative: pushing forward walks away from the camera, and
-- the "Model" child smoothly turns to face where you're going. Animation states
-- are driven from the body's real velocity + grounded flag each frame — calls
-- like anim:play("Run") are safe every frame (re-playing never restarts).

defaults = {
  walk = 4.5,
  run = 8.0,
  double_tap = 0.3, -- max seconds between taps to trigger a run
  jump = 7.0,
  turn = 12.0,      -- how quickly the model turns to face movement (rad/s-ish)
  walk_anim_at = 0.4, -- speed above which Walk plays
  run_anim_at = 6.0,  -- speed above which Run plays
  ground_ray = 1.5,   -- downward probe length for the forgiving ground check
  --@slider 20 85 --@step 1 --@units degrees
  -- The steepest ground you can walk UP. Anything steeper you slide off
  -- instead of climbing — which is also what stops a run at a cliff from
  -- firing you into the sky.
  slope_limit = 50,
  -- Draws the ground probe in the Scene view: green grounded, red airborne.
  debug_ray = false,
}

local anim        -- animator on the visual child (or this node)
local model       -- the "Model" child we rotate to face movement
local cam         -- the third_person_camera script (for camera-relative moves)
local heading = 0 -- the model's current facing (smoothed)
local states      -- resolved animation state names (see resolve_states)
local rig
local running = false  -- run mode: armed by a double-tap
local lastTap = -10    -- when movement last STARTED (for double-tap detection)
local wasMoving = false

local function normalize(x, y, z)
  local l = math.sqrt(x * x + y * y + z * z)
  if l < 1e-6 then return 0, 0, 0 end
  return x / l, y / l, z / l
end

-- Don't push into a surface you can't walk up.
--
-- The solver resolves an overlap by pushing the capsule out along the surface
-- normal; on a steep face that normal points partly UP, so a controller that
-- keeps driving into it gets that push again every frame — which is what fires
-- a character into the sky at the foot of a cliff. Take the into-the-surface
-- part out of the movement and what's left is a slide along it.
--
-- `n` is a contact normal (`node.wallNormal` / `node.groundNormal`, either may
-- be nil), `u*` is the body's up, `steep` is cos(slope limit).
local function slide(mx, my, mz, n, ux, uy, uz, steep)
  if not n then return mx, my, mz end
  if n.x * ux + n.y * uy + n.z * uz >= steep then return mx, my, mz end -- walkable
  local into = mx * n.x + my * n.y + mz * n.z
  if into >= 0 then return mx, my, mz end -- already moving away from it
  return mx - n.x * into, my - n.y * into, mz - n.z * into
end

-- The controller's real state names can differ from the classic four — a
-- re-exported clip often lands as "Idle.001". Match each want exactly first,
-- then case-insensitively, then by prefix, so anim:play always hits a state.
local function resolve_states()
  local names = anim and anim:clips() or nil
  if not names or #names == 0 then return nil end
  local function pick(want)
    local lw = string.lower(want)
    local best = nil
    for _, n in ipairs(names) do
      local ln = string.lower(n)
      if ln == lw then return n end
      if not best and string.sub(ln, 1, #lw) == lw then best = n end
    end
    return best or want
  end
  return { idle = pick("Idle"), walk = pick("Walk"), run = pick("Run"), jump = pick("Jump") }
end

function start(node)
  model = node:find("Model")
  local vis = model or node
  anim = vis:animator()
  cam = findScript("third_person_camera")
  heading = (model and model.yaw) or node.yaw
  rig = node:getcomponent("RigidBody")
end

function update(node, dt)
  -- Resolve state names once the animator has bound (its clip list is empty on
  -- the very first frame).
  if anim and not states then states = resolve_states() end

  -- Move relative to the VIEW yaw. input.aimYaw() is the active camera's yaw
  -- captured with the input snapshot — in multiplayer it rides the input
  -- command, so the server and prediction replay use EXACTLY the angle you
  -- saw (reading the camera node directly can never match across machines).
  local yaw = input.aimYaw()
  if not yaw then
    yaw = node.yaw
    if cam and cam.node and cam.node.valid then yaw = cam.node.yaw end
  end

  -- "up" = −gravity (works on flat worlds and radial planets alike).
  local ux, uy, uz = node.up_x, node.up_y, node.up_z

  local cy, sy = math.cos(yaw), math.sin(yaw)
  local fx, fy, fz = -sy, 0.0, -cy
  local rx, ry, rz = cy, 0.0, -sy
  local fd = fx * ux + fy * uy + fz * uz
  fx, fy, fz = normalize(fx - ux * fd, fy - uy * fd, fz - uz * fd)
  local rd = rx * ux + ry * uy + rz * uz
  rx, ry, rz = normalize(rx - ux * rd, ry - uy * rd, rz - uz * rd)

  -- Movement: WASD or the left stick. Already deadzoned, SOCD-resolved and
  -- clamped to the unit disk, so there's nothing left to normalise.
  local s, f = input.axis2("Move")

  -- RUN, two ways. Holding Sprint (Shift / L3) is the obvious one. The other
  -- is a double-tap — detected on the MOVE AXIS starting from rest rather than
  -- on a key edge, so a stick flick arms it exactly like tapping W does.
  -- Releasing all movement drops back to a walk.
  local moving = (f ~= 0 or s ~= 0)
  if moving and not wasMoving then
    if time - lastTap < params.double_tap then running = true end
    lastTap = time
  end
  wasMoving = moving
  if not moving then running = false end

  local speed = (running or input.action("Sprint")) and params.run or params.walk

  -- Grounding, with forgiveness: the physics contact flag OR a short ray
  -- straight down — running down a slope leaves the ground for a few frames
  -- and shouldn't rob you of a jump (or flicker the Jump animation).
  local grounded = node.grounded
  if not grounded and params.ground_ray > 0 then
    grounded = raycast(node.x, node.y, node.z, -ux, -uy, -uz, params.ground_ray) ~= nil
  end

  -- Debug view of that probe, drawn with the `gizmo` API (immediate mode — call
  -- it every frame you want it visible): green while grounded, red in the air.
  if params.debug_ray and params.ground_ray > 0 then
    if grounded then
      gizmo.ray(node.x, node.y, node.z, -ux, -uy, -uz, params.ground_ray, 0.3, 1.0, 0.4)
    else
      gizmo.ray(node.x, node.y, node.z, -ux, -uy, -uz, params.ground_ray, 1.0, 0.35, 0.3)
    end
  end

  -- Keep the vertical (gravity/jump) part, steer the horizontal part.
  local vup = node.vx * ux + node.vy * uy + node.vz * uz
  local startingJump = false
  if grounded and input.justPressed("Jump") then
    vup = params.jump
    startingJump = true
  elseif node.grounded and vup > 0 then
    -- Standing on something and moving UP without having jumped: that came
    -- from being pushed out of a slope or a step, not from you. Keeping it is
    -- how a walk turns into a takeoff. (Downward is kept — that's gravity.)
    vup = 0
  end
  local mx = (fx * f + rx * s) * speed
  local my = (fy * f + ry * s) * speed
  local mz = (fz * f + rz * s) * speed
  -- Slopes: refuse to climb anything steeper than `slope_limit`, and slide
  -- along it instead. `wallNormal` is the steepest surface the body is pressed
  -- against, `groundNormal` is what it's standing on.
  local steep = math.cos(math.rad(params.slope_limit))
  mx, my, mz = slide(mx, my, mz, node.wallNormal, ux, uy, uz, steep)
  mx, my, mz = slide(mx, my, mz, node.groundNormal, ux, uy, uz, steep)
  node.vx = mx + ux * vup
  node.vy = my + uy * vup
  node.vz = mz + uz * vup

  -- Turn the visual (the capsule itself never rotates). SHIFT LOCK (or first
  -- person — read from the camera script's exposed state) faces the CAMERA's
  -- yaw, Y axis only, even while standing still; otherwise face travel. The
  -- camera script is local to this machine, so in multiplayer the server's
  -- replay may face a remote model slightly differently — cosmetic only (the
  -- movement itself uses input.aimYaw(), which rides the input command).
  if not cam then cam = findScript("third_person_camera") end
  local locked = cam and (cam.shiftlock or cam.firstPerson)
  if model and (moving or locked) then
    local want
    if locked then
      want = yaw                      -- face away from the camera (its view yaw)
    else
      want = math.atan2(-mx, -mz)     -- engine forward is −Z (atan2: LuaJIT/5.1)
    end
    local diff = want - heading
    while diff > math.pi do diff = diff - 2 * math.pi end
    while diff < -math.pi do diff = diff + 2 * math.pi end
    local step = math.min(1.0, params.turn * dt)
    heading = heading + diff * step
    model.yaw = heading
  end
  
  -- Stand still and STAY still: full friction while planted and not asking to
  -- move, so a gentle slope doesn't slide you downhill. The moment you do ask
  -- to move, drop it — friction fights your own walk speed otherwise, and
  -- pressing into a wall with grip is what makes a character stick to it.
  if rig then
    rig.friction = (not moving) and node.grounded and 1.0 or 0.0
  end

  -- Drive the animation from what the body is actually doing (the forgiving
  -- grounded flag keeps Jump from flickering on downhill runs).
  if anim and states then
    local hspeed = math.sqrt(mx * mx + my * my + mz * mz)
    if not grounded then
      anim:play(states.jump)
    elseif hspeed > params.run_anim_at then
      anim:play(states.run)
    elseif hspeed > params.walk_anim_at then
      anim:play(states.walk)
    else
      anim:play(states.idle)
    end
  end
end
