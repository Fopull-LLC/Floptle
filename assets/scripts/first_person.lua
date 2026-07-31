-- First-person character controller (the default FPS setup).
-- Works on a keyboard, on a gamepad, or on both at once.
--
-- SETUP: make a Camera node, mark it Active, give it a *Capsule* Rigidbody
-- (Inspector → ◆ Rigidbody → shape: Capsule), then attach this script. On Play
-- you ARE that capsule: it moves under physics and the camera rides along.
--
--   Look     hold RIGHT MOUSE to free-look, or use the RIGHT STICK (always live)
--   Move     WASD or the left stick — along the ground, relative to where you face
--   Jump     Space or the pad's South button (only when grounded)
--   Sprint   Shift or L3 (hold) — run
--   Crouch   C or the pad's East button (hold) — shrinks the capsule and slows you
--
-- Every control is a NAMED ACTION from Project Settings → Input, so this script
-- never asks which device you are on, and any of it can be rebound without
-- touching the code.
--
-- It is genuinely rig-driven: each frame it reads the body's own velocity /
-- grounded / up from the physics sim, modifies the velocity, and writes it
-- back for the engine to integrate. Works with normal Down gravity AND Radial
-- (planet) gravity — movement follows the surface tangent and jump uses the
-- body's up (−gravity), so you can run all the way around a planet.
--
-- Want a shoulder camera instead? Use third_person.lua + third_person_camera.lua.

defaults = {
  --@header Speed
  --@range 0 20 --@units m/s
  walk = 6.0,
  --@range 0 30 --@units m/s
  run = 10.0,
  --@range 0 20 --@units m/s
  crouch_walk = 3.0,
  --@header Jumping & slopes
  -- Upward speed a jump starts with — about 2.5 m of height at default gravity.
  --@range 0 30 --@units m/s
  jump = 7.0,
  -- The steepest ground you can walk UP. Anything steeper you slide off
  -- instead of climbing, which is also what stops a run at a cliff from firing
  -- you into the sky.
  --@slider 20 85 --@step 1 --@units degrees
  slope_limit = 50,
  -- Downward probe for the forgiving ground check: running down a slope leaves
  -- the ground for a few frames and shouldn't rob you of a jump.
  --@range 0 5 --@units m
  ground_ray = 1.5,
  --@header Look
  --@slider 0.1 4 --@step 0.05
  sensitivity = 1.0,
  --@header Capsule
  --@range 0.5 4 --@units m
  stand_height = 2.0,
  --@range 0.5 4 --@units m
  crouch_height = 1.1,
  --@header Debug
  -- Draw the ground probe in the Scene view: green grounded, red airborne.
  debug_ray = false,
}

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

function update(node, dt)
  -- One "Look" axis serves mouse and stick alike: the mouse half is gated on
  -- the right button in the input map (a free cursor must not spin the view),
  -- the stick half is always live. Both arrive as radians per second, so a
  -- single `* dt` is correct — and frame-rate independent — for either.
  local lx, ly = input.axis2("Look")
  input.setMouseLocked(input.action("LookEnable"))
  node.yaw = node.yaw - lx * params.sensitivity * dt
  node.pitch = node.pitch - ly * params.sensitivity * dt
  local lim = math.pi * 0.5 - 0.02 -- don't let the view flip over
  if node.pitch > lim then node.pitch = lim end
  if node.pitch < -lim then node.pitch = -lim end

  -- "up" = −gravity (Y on a flat world, radial on a planet).
  local ux, uy, uz = node.up_x, node.up_y, node.up_z

  -- Forward/right from YAW only (engine forward = −Z), flattened onto the
  -- surface so you move along the ground even while looking up or down.
  local cy, sy = math.cos(node.yaw), math.sin(node.yaw)
  local fx, fy, fz = -sy, 0.0, -cy
  local rx, ry, rz = cy, 0.0, -sy
  local fd = fx * ux + fy * uy + fz * uz
  fx, fy, fz = normalize(fx - ux * fd, fy - uy * fd, fz - uz * fd)
  local rd = rx * ux + ry * uy + rz * uz
  rx, ry, rz = normalize(rx - ux * rd, ry - uy * rd, rz - uz * rd)

  -- Movement: WASD or the left stick. Already deadzoned, SOCD-resolved, and
  -- clamped to the unit disk, so diagonals aren't faster and there is nothing
  -- left to normalise here.
  local s, f = input.axis2("Move")

  -- Crouch: the engine resizes the capsule, feet planted — so the camera dips
  -- and your feet stay where they were. The height you write WINS over the
  -- Rigidbody's authored one for as long as you keep writing it, so these two
  -- params are the real thing, and either can be anything you like.
  local crouching = input.action("Crouch")
  -- Standing up under a ledge would put your head through it: if there isn't
  -- room overhead, stay down until there is.
  if not crouching and params.stand_height > params.crouch_height then
    local room = (params.stand_height - params.crouch_height) + 0.1
    crouching = raycast(node.x, node.y, node.z, ux, uy, uz, params.crouch_height * 0.5 + room)
      ~= nil
  end
  if crouching then node.height = params.crouch_height else node.height = params.stand_height end

  local speed = params.walk
  if crouching then
    speed = params.crouch_walk
  elseif input.action("Sprint") then
    speed = params.run
  end

  -- Grounding, with forgiveness: the physics contact flag OR a short ray
  -- straight down — running down a slope leaves the ground for a few frames
  -- and shouldn't rob you of a jump.
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

  -- READ the body's velocity, keep its vertical (gravity/jump) part, MODIFY
  -- the horizontal part, WRITE it back — physics integrates it next step.
  local vup = node.vx * ux + node.vy * uy + node.vz * uz
  if grounded and input.justPressed("Jump") then
    vup = params.jump
  elseif node.grounded and vup > 0 then
    -- Standing on something and moving UP without having jumped: that speed
    -- came from being pushed out of a slope or a step, not from you. Keeping
    -- it is how a walk turns into a takeoff. (Downward is kept — that's
    -- gravity holding you against the ground.)
    vup = 0
  end

  local mx = (fx * f + rx * s) * speed
  local my = (fy * f + ry * s) * speed
  local mz = (fz * f + rz * s) * speed

  -- Slopes: refuse to climb anything steeper than `slope_limit` and slide
  -- along it instead. `wallNormal` is the steepest surface the body is pressed
  -- against (the cliff you ran at); `groundNormal` is what it's standing on.
  local steep = math.cos(math.rad(params.slope_limit))
  mx, my, mz = slide(mx, my, mz, node.wallNormal, ux, uy, uz, steep)
  mx, my, mz = slide(mx, my, mz, node.groundNormal, ux, uy, uz, steep)

  node.vx = mx + ux * vup
  node.vy = my + uy * vup
  node.vz = mz + uz * vup
end
