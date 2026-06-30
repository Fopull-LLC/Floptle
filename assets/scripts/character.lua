-- First-person character controller.
--
-- SETUP: make a Camera node, mark it Active, give it a *Capsule* Rigidbody (Inspector →
-- ◆ Rigidbody → shape: Capsule), then attach this script. On Play you ARE that capsule:
-- it moves under physics and the camera rides along, so you walk the world first-person.
-- (You can also attach it to any capsule rig for a third-person body — it still drives it.)
--
--   hold RIGHT MOUSE   free-look (yaw + pitch)
--   W A S D            move along the ground, relative to where you face
--   SPACE              jump (only when grounded)
--   SHIFT (hold)       run
--   C (hold)           crouch — shrinks the capsule (you duck) and slows you
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
  sensitivity = 0.3,
  stand_height = 2.0,
  crouch_height = 1.1,
}

local function normalize(x, y, z)
  local l = math.sqrt(x * x + y * y + z * z)
  if l < 1e-6 then return 0, 0, 0 end
  return x / l, y / l, z / l
end

function update(node, dt)
  -- Free-look while holding right mouse (yaw turns, pitch looks up/down).
  if input.button(1) then
    local dx, dy = input.mouse_delta()
    node.yaw = node.yaw - dx * params.sensitivity * 0.01
    node.pitch = node.pitch - dy * params.sensitivity * 0.01
    local lim = math.pi * 0.5 - 0.02 -- don't let the view flip over
    if node.pitch > lim then node.pitch = lim end
    if node.pitch < -lim then node.pitch = -lim end
  end

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

  -- Movement input (normalized so diagonals aren't faster).
  local f, s = 0, 0
  if input.key("w") then f = f + 1 end
  if input.key("s") then f = f - 1 end
  if input.key("d") then s = s + 1 end
  if input.key("a") then s = s - 1 end
  local il = math.sqrt(f * f + s * s)
  if il > 1 then f, s = f / il, s / il end

  -- Crouch: ask the engine to resize the capsule (it keeps the feet planted, so you
  -- duck). Releasing C stands back up.
  local crouching = input.key("c")
  if crouching then node.height = params.crouch_height else node.height = params.stand_height end

  local speed = params.walk
  if crouching then
    speed = params.crouch_walk
  elseif input.key("shift") then
    speed = params.run
  end

  -- READ the rig's current velocity, keep its vertical (gravity/jump) part, MODIFY the
  -- horizontal part, then WRITE it back — the engine integrates it next physics step.
  local vup = node.vx * ux + node.vy * uy + node.vz * uz
  if node.grounded and input.pressed("space") then
    vup = params.jump
  end

  local mx = (fx * f + rx * s) * speed
  local my = (fy * f + ry * s) * speed
  local mz = (fz * f + rz * s) * speed

  node.vx = mx + ux * vup
  node.vy = my + uy * vup
  node.vz = mz + uz * vup
end
