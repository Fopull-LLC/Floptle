-- Basic physics character controller.
--
-- Attach this to a node that ALSO has a Rigidbody (a capsule works best). On Play it
-- reads the body's velocity + grounded state and drives it from input:
--   hold RIGHT MOUSE   turn (look left/right)
--   WASD               move along the ground (relative to where you face)
--   Space              jump (only when grounded)
--
-- Works with normal Down gravity AND Radial (planet) gravity: it moves along the
-- surface tangent and jumps along the body's "up" (−gravity), so you can run all the
-- way around a planet.

defaults = { speed = 6, jump = 7, sensitivity = 0.3 }

local function normalize(x, y, z)
  local l = math.sqrt(x * x + y * y + z * z)
  if l < 1e-6 then return 0, 0, 0 end
  return x / l, y / l, z / l
end

function update(node, dt)
  -- "up" = −gravity (Y on a flat world, radial on a planet).
  local ux, uy, uz = node.up_x, node.up_y, node.up_z

  -- Turn while holding right mouse.
  if input.button(1) then
    local dx, _ = input.mouse_delta()
    node.yaw = node.yaw - dx * params.sensitivity * 0.01
  end

  -- Forward/right from yaw (engine: forward = −Z), flattened onto the surface tangent.
  local cy, sy = math.cos(node.yaw), math.sin(node.yaw)
  local fx, fy, fz = -sy, 0.0, -cy
  local rx, ry, rz = cy, 0.0, -sy
  local fd = fx * ux + fy * uy + fz * uz
  fx, fy, fz = normalize(fx - ux * fd, fy - uy * fd, fz - uz * fd)
  local rd = rx * ux + ry * uy + rz * uz
  rx, ry, rz = normalize(rx - ux * rd, ry - uy * rd, rz - uz * rd)

  local f, s = 0, 0
  if input.key("w") then f = f + 1 end
  if input.key("s") then f = f - 1 end
  if input.key("d") then s = s + 1 end
  if input.key("a") then s = s - 1 end

  -- Keep the velocity's vertical (gravity/jump) part; replace the horizontal part.
  local vup = node.vx * ux + node.vy * uy + node.vz * uz
  if node.grounded and input.pressed("space") then
    vup = params.jump
  end

  local mx = (fx * f + rx * s) * params.speed
  local my = (fy * f + ry * s) * params.speed
  local mz = (fz * f + rz * s) * params.speed

  node.vx = mx + ux * vup
  node.vy = my + uy * vup
  node.vz = mz + uz * vup
end
