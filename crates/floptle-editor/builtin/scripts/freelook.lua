-- Free-look fly camera. Works on a keyboard, on a gamepad, or on both at once.
--
--   MOUSE    hold RIGHT MOUSE to look — the cursor stays free otherwise, so the
--            view never spins on its own
--   PAD      the RIGHT STICK looks, always (a stick recentres itself, so there
--            is nothing to hold)
--   Move     WASD or the left stick
--   Fly      Space / Ctrl, or the triggers
--   Sprint   Shift or L3 — move faster
--
-- Every control above is a NAMED ACTION from Project Settings → Input, so this
-- script never asks which device you are on, and you can rebind any of it
-- without touching the code.
--
-- Attach to a Camera node and make that camera active. The default new-scene
-- camera ships with it already attached, so pressing Play lets you fly the shot.

defaults = {
  speed = 8,          -- movement units per second
  boost = 3,          -- Sprint multiplier
  sensitivity = 1.0,  -- look speed multiplier (tune per-camera here)
}

local PITCH_LIMIT = math.pi * 0.5 - 0.02 -- stop just short of straight up/down

function update(node, dt)
  -- One axis, both devices. The mouse half is gated on the right button in the
  -- input map; the stick half is not, because a stick returns to centre by
  -- itself. Both arrive as a RATE (radians per second), so one `* dt` is
  -- correct for either — and identical at 30 fps and 240 fps.
  local lx, ly = input.axis2("Look")
  -- Capture the cursor only while the MOUSE is the thing looking.
  input.setMouseLocked(input.action("LookEnable"))
  node.yaw = node.yaw - lx * params.sensitivity * dt
  node.pitch = node.pitch - ly * params.sensitivity * dt
  if node.pitch > PITCH_LIMIT then node.pitch = PITCH_LIMIT end
  if node.pitch < -PITCH_LIMIT then node.pitch = -PITCH_LIMIT end

  -- Orientation basis (matches the engine's YXZ camera: forward = -Z).
  local cy, sy = math.cos(node.yaw), math.sin(node.yaw)
  local cp, sp = math.cos(node.pitch), math.sin(node.pitch)
  local fx, fy, fz = -cp * sy, sp, -cp * cy -- forward (where you look)
  local rx, rz = cy, -sy                    -- right (horizontal strafe)

  -- Movement: WASD or the left stick, already deadzoned and normalised so
  -- diagonals aren't faster. `rise` is Space/Ctrl or the triggers.
  local strafe, fwd = input.axis2("Move")
  local rise = input.axis1("Fly")

  local speed = params.speed
  if input.action("Sprint") then speed = speed * params.boost end
  local step = speed * dt

  node.x = node.x + (fx * fwd + rx * strafe) * step
  node.y = node.y + fy * fwd * step + rise * step
  node.z = node.z + (fz * fwd + rz * strafe) * step
end
