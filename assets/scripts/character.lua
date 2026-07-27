-- First-person character controller.
--
-- SETUP: make a Camera node, mark it Active, give it a *Capsule* Rigidbody (Inspector →
-- ◆ Rigidbody → shape: Capsule), then attach this script. On Play you ARE that capsule:
-- it moves under physics and the camera rides along, so you walk the world first-person.
-- (You can also attach it to any capsule rig for a third-person body — it still drives it.)
--
--   Look     hold RIGHT MOUSE to free-look, or use the RIGHT STICK (always live)
--   Move     WASD or the left stick — along the ground, relative to where you face
--   Jump     Space or the pad's South button (only when grounded)
--   Sprint   Shift or L3 (hold) — run
--   Crouch   C or the pad's East button (hold) — shrinks the capsule and slows you
--
-- Every control is a NAMED ACTION from Project Settings → Input, so this works on a
-- keyboard, on a gamepad, or on both at once.
--
-- It is genuinely rig-driven: each frame it reads the body's own velocity / grounded /
-- up from the physics sim, modifies the velocity, and writes it back for the engine to
-- integrate. Works with normal Down gravity AND Radial (planet) gravity — movement
-- follows the surface tangent and jump uses the body's up (−gravity), so you can run all
-- the way around a Mario-Galaxy planet.

defaults = {
  walk = 6.0,
  run = 10.0,
  crouch_walk = 3.0,
  jump = 7.0,
  sensitivity = 1.0,  -- look speed multiplier
  stand_height = 2.0,
  crouch_height = 1.1,
}

local function normalize(x, y, z)
  local l = math.sqrt(x * x + y * y + z * z)
  if l < 1e-6 then return 0, 0, 0 end
  return x / l, y / l, z / l
end

function update(node, dt)
  -- One "Look" axis serves mouse and stick alike: the mouse half is gated on the
  -- right button in the input map (a free cursor must not spin the view), the stick
  -- half is always live. Both arrive as radians per second, so one `* dt` is correct
  -- for either — and identical at any framerate.
  local lx, ly = input.axis2("Look")
  input.setMouseLocked(input.action("LookEnable"))
  node.yaw = node.yaw - lx * params.sensitivity * dt
  node.pitch = node.pitch - ly * params.sensitivity * dt
  local lim = math.pi * 0.5 - 0.02 -- don't let the view flip over
  if node.pitch > lim then node.pitch = lim end
  if node.pitch < -lim then node.pitch = -lim end
	
  -- "up" = −gravity (Y on a flat world, radial on a planet).
  local ux, uy, uz = node.up_x, node.up_y, node.up_z

  -- Forward/right from YAW only (engine forward = −Z), flattened onto the surface so you
  -- move along the ground instead of flying when you look up or down.
  local cy, sy = math.cos(node.yaw), math.sin(node.yaw)
  local fx, fy, fz = -sy, 0.0, -cy
  local rx, ry, rz = cy, 0.0, -sy
  local fd = fx * ux + fy * uy + fz * uz
  fx, fy, fz = normalize(fx - ux * fd, fy - uy * fd, fz - uz * fd)
  local rd = rx * ux + ry * uy + rz * uz
  rx, ry, rz = normalize(rx - ux * rd, ry - uy * rd, rz - uz * rd)

  -- Movement: WASD or the left stick. Already deadzoned, SOCD-resolved and clamped
  -- to the unit disk, so diagonals aren't faster and nothing is left to normalise.
  local s, f = input.axis2("Move")

  -- Crouch: ask the engine to resize the capsule (it keeps the feet planted, so you
  -- duck). Releasing it stands back up.
  local crouching = input.action("Crouch")
  if crouching then node.height = params.crouch_height else node.height = params.stand_height end

  local speed = params.walk
  if crouching then
    speed = params.crouch_walk
  elseif input.action("Sprint") then
    speed = params.run
  end

  -- READ the rig's current velocity, keep its vertical (gravity/jump) part, MODIFY the
  -- horizontal part, then WRITE it back — the engine integrates it next physics step.
  local vup = node.vx * ux + node.vy * uy + node.vz * uz
  if node.grounded and input.justPressed("Jump") then
    vup = params.jump
  end

  local mx = (fx * f + rx * s) * speed
  local my = (fy * f + ry * s) * speed
  local mz = (fz * f + rz * s) * speed

  node.vx = mx + ux * vup
  node.vy = my + uy * vup
  node.vz = mz + uz * vup
end
