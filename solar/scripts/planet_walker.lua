-- Planet-surface character controller (pairs with planet_camera.lua).
--
-- Movement happens in the tangent plane of the body's LOCAL up (−gravity), with
-- the forward direction taken from the planet camera's published view basis — so
-- W always walks "away from the camera" no matter where on the planet you stand.
-- The body's Rigidbody should have "align to gravity" ON so the capsule (and any
-- children) tilt with the surface.
--
--   W A S D   move (camera-relative)      SPACE  jump (when grounded)
--   DOUBLE-TAP a move key to RUN

defaults = {
  walk = 4.5,
  run = 8.0,
  double_tap = 0.3,
  jump = 7.0,
  ground_ray = 1.6,
}

local cam
local ship
local rig
local running = false
local tap = { w = -10, a = -10, s = -10, d = -10 }

local function norm(x, y, z)
  local l = math.sqrt(x * x + y * y + z * z)
  if l < 1e-6 then return 0, 0, 0 end
  return x / l, y / l, z / l
end

function start(node)
  rig = node:getcomponent("RigidBody")
  cam = findScript("planet_camera")
end

function update(node, dt)
  if not cam then cam = findScript("planet_camera") end
  -- Hands off while flying the ship (it parks + carries this body itself).
  if not ship then ship = findScript("ship_controller") end
  if ship and ship.piloting then return end
  -- …or a BUILT vessel (fetched fresh: vessels spawn and despawn at runtime),
  -- or while a launch handoff is waiting to seat us in the pod.
  local vessel = findScript("vessel_controller")
  if vessel and vessel.piloting then return end
  if (save.get("shipyard.pilot") or 0) == 1 then return end
  -- Nothing is piloting us anywhere: we must be walkable AND visible (a
  -- destroyed vessel can't un-hide the astronaut it seated — self-heal here).
  if not node.visible then node.visible = true end

  -- Local up: the body's −gravity axis (fallback: radially out from origin).
  local ux, uy, uz = node.up_x, node.up_y, node.up_z
  if not ux or (ux == 0 and uy == 0 and uz == 0) then
    ux, uy, uz = norm(node.x, node.y, node.z)
    if ux == 0 and uy == 0 and uz == 0 then ux, uy, uz = 0, 1, 0 end
  end

  -- Forward = the camera's view projected onto the tangent plane (published by
  -- planet_camera as flat_*). Fallback: aim yaw projected, like third_person.
  local fx, fy, fz
  if cam and cam.flat_x then
    fx, fy, fz = cam.flat_x, cam.flat_y, cam.flat_z
  else
    local yaw = input.aimYaw() or 0.0
    fx, fy, fz = -math.sin(yaw), 0.0, -math.cos(yaw)
    local fd = fx * ux + fy * uy + fz * uz
    fx, fy, fz = norm(fx - ux * fd, fy - uy * fd, fz - uz * fd)
  end
  if fx == 0 and fy == 0 and fz == 0 then return end
  -- right = forward × up (tangent, ⊥ forward).
  local rx = fy * uz - fz * uy
  local ry = fz * ux - fx * uz
  local rz = fx * uy - fy * ux

  local f, s = 0, 0
  if input.key("w") then f = f + 1 end
  if input.key("s") then f = f - 1 end
  if input.key("d") then s = s + 1 end
  if input.key("a") then s = s - 1 end
  local il = math.sqrt(f * f + s * s)
  if il > 1 then f, s = f / il, s / il end

  for _, k in ipairs({ "w", "a", "s", "d" }) do
    if input.pressed(k) then
      if time - tap[k] < params.double_tap then running = true end
      tap[k] = time
    end
  end
  if f == 0 and s == 0 then running = false end
  local speed = running and params.run or params.walk

  -- Grounding with forgiveness: contact flag OR a short probe along −up.
  local grounded = node.grounded
  if not grounded and params.ground_ray > 0 then
    grounded = raycast(node.x, node.y, node.z, -ux, -uy, -uz, params.ground_ray, node) ~= nil
  end

  -- Steer the tangent part, keep (or set) the up part.
  local vup = node.vx * ux + node.vy * uy + node.vz * uz
  if grounded and input.pressed("space") then vup = params.jump end
  local mx = (fx * f + rx * s) * speed
  local my = (fy * f + ry * s) * speed
  local mz = (fz * f + rz * s) * speed
  node.vx = mx + ux * vup
  node.vy = my + uy * vup
  node.vz = mz + uz * vup

  -- Standing still on a slope: max friction so you don't slide downhill.
  if rig then
    if f == 0 and s == 0 and grounded then
      rig.friction = 1
    else
      rig.friction = 0
    end
  end
end
