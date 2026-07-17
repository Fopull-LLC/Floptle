-- Excavate & build on the planetoid — the Terrain 2.0 runtime API in action.
-- Hold LMB to dig where you aim; hold Q to pile ground back up.
-- (RMB stays free: the camera uses it to look around.)
defaults = { radius = 2.2, strength = 0.85, range = 28.0 }

function update(node, dt)
  local dig = input.button(0)
  local build = input.key("q")
  if not (dig or build) then return end

  -- Aim ray from the camera's yaw/pitch (aim rides the input command, so this
  -- stays correct under netcode prediction too).
  local yaw, pitch = input.aimYaw(), input.aimPitch()
  local cp = math.cos(pitch)
  local dx, dy, dz = -math.sin(yaw) * cp, math.sin(pitch), -math.cos(yaw) * cp
  local h = raycast(node.x, node.y + 0.8, node.z, dx, dy, dz, params.range, node)
  if not h then return end

  if dig then
    terrain.dig(h.x, h.y, h.z, params.radius, params.strength)
  else
    terrain.sculpt(h.x, h.y, h.z, params.radius, params.strength, "raise")
  end
end
