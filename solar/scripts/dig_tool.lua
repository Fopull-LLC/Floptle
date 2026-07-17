-- Excavate & build on the planetoid — the Terrain 2.0 runtime API in action.
-- Hold LMB to dig where you aim, Q to pile ground back up.
--
-- The ray starts AT THE CAMERA along its exact view line (read from
-- planet_camera's published basis), so digs land on the crosshair line — casting
-- from the character instead puts a third-person parallax offset on every dig.
-- Dabs are SPACED like the editor brush (move ⅓ radius or wait) instead of
-- landing every frame, so holding the button carves steadily, not explosively.

defaults = { radius = 1.3, strength = 0.6, range = 30.0, spacing = 0.45, rate = 0.12 }

local cam
local last_x, last_y, last_z = nil, nil, nil
local last_t = -10.0

function start(node)
  cam = findScript("planet_camera")
end

function update(node, dt)
  local dig = input.button(0)
  local build = input.key("q")
  if not (dig or build) then
    last_x = nil -- next press dabs immediately
    last_t = -10.0
    return
  end
  if not cam then
    cam = findScript("planet_camera")
    if not cam then return end
  end

  -- Aim: the camera's own position + view direction = the crosshair line.
  local ox, oy, oz = cam.cam_x, cam.cam_y, cam.cam_z
  local dx, dy, dz = cam.fwd_x, cam.fwd_y, cam.fwd_z
  if not (ox and dx) then return end
  local h = raycast(ox, oy, oz, dx, dy, dz, params.range, node)
  if not h then return end

  -- Space the dabs: far enough from the last one, or held long enough in place.
  local moved = true
  if last_x then
    local ddx, ddy, ddz = h.x - last_x, h.y - last_y, h.z - last_z
    moved = (ddx * ddx + ddy * ddy + ddz * ddz)
      >= (params.radius * params.spacing) ^ 2
  end
  if not moved and (time - last_t) < params.rate then return end
  last_x, last_y, last_z, last_t = h.x, h.y, h.z, time

  if dig then
    terrain.dig(h.x, h.y, h.z, params.radius, params.strength)
  else
    terrain.sculpt(h.x, h.y, h.z, params.radius, params.strength, "raise")
  end
end
