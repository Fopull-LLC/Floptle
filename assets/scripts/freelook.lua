-- Free-look fly camera.
--
-- Hold the RIGHT MOUSE button to look around; the cursor stays free otherwise so
-- the view never spins on its own. While looking:
--   WASD          move (relative to where you look)
--   Space / Ctrl  rise / descend
--   Shift         move faster (boost)
--
-- Attach this to a Camera node and make that camera active. The default new-scene
-- camera ships with it already attached, so pressing Play lets you fly the shot.

defaults = {
  speed = 8,          -- movement units per second
  boost = 3,          -- Shift multiplier
  sensitivity = 0.3,  -- mouse-look (~radians per 30 px of motion)
}

local PITCH_LIMIT = math.pi * 0.5 - 0.02 -- stop just short of straight up/down

function update(node, dt)
  -- Look only while the right mouse button is held.
  if input.button(1) then
    local dx, dy = input.mouse_delta()
    local s = params.sensitivity * 0.01
    node.yaw = node.yaw - dx * s         -- drag right -> turn right
    node.pitch = node.pitch - dy * s     -- drag up    -> look up
    if node.pitch > PITCH_LIMIT then node.pitch = PITCH_LIMIT end
    if node.pitch < -PITCH_LIMIT then node.pitch = -PITCH_LIMIT end
  end

  -- Orientation basis (matches the engine's YXZ camera: forward = -Z).
  local cy, sy = math.cos(node.yaw), math.sin(node.yaw)
  local cp, sp = math.cos(node.pitch), math.sin(node.pitch)
  local fx, fy, fz = -cp * sy, sp, -cp * cy -- forward (where you look)
  local rx, rz = cy, -sy                    -- right (horizontal strafe)

  -- Gather movement input.
  local fwd = 0
  if input.key("w") then fwd = fwd + 1 end
  if input.key("s") then fwd = fwd - 1 end
  local strafe = 0
  if input.key("d") then strafe = strafe + 1 end
  if input.key("a") then strafe = strafe - 1 end
  local rise = 0
  if input.key("space") then rise = rise + 1 end
  if input.key("ctrl") then rise = rise - 1 end

  local speed = params.speed
  if input.key("shift") then speed = speed * params.boost end
  local step = speed * dt

  node.x = node.x + (fx * fwd + rx * strafe) * step
  node.y = node.y + fy * fwd * step + rise * step
  node.z = node.z + (fz * fwd + rz * strafe) * step
end
