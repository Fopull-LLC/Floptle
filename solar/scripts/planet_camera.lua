-- Planet-aware third-person orbit camera (pairs with planet_walker.lua).
--
-- The stock third_person_camera orbits with WORLD yaw/pitch — on a radial-gravity
-- planet that frame stops matching "up" the moment you leave the pole, and the view
-- fights you. This one orbits in the CHARACTER'S local frame: up = the body's
-- −gravity up, and the yaw reference is parallel-transported as you walk around the
-- sphere, so the horizon stays level all the way around the planet.
--
--   RIGHT MOUSE (hold)  orbit          SCROLL  zoom          SHIFT  toggle lock
--
-- Exposes its view basis (env globals) for the walker + dig tool:
--   fwd_x/fwd_y/fwd_z     the exact view direction (world space)
--   flat_x/flat_y/flat_z  that direction projected onto the tangent plane
--   cam_x/cam_y/cam_z     the camera position
--
-- ATTACH to a TOP-LEVEL Camera node (NOT parented to the player: script node
-- coordinates are parent-local, so a moving parent would double-apply).

defaults = {
  distance = 7.0,
  min_distance = 1.5,
  max_distance = 14.0,
  height = 1.2,        -- look-at point above the character's origin, along up
  sensitivity = 0.3,
  zoom_speed = 1.0,
  start_pitch = -0.3,
}

shiftlock = false
-- View basis for other scripts (world space; set every frame).
fwd_x, fwd_y, fwd_z = 0.0, 0.0, -1.0
flat_x, flat_y, flat_z = 0.0, 0.0, -1.0
cam_x, cam_y, cam_z = 0.0, 0.0, 0.0

local target
local pitch = nil
-- The yaw reference direction, parallel-transported across the planet surface.
local rx, ry, rz = 0.0, 0.0, -1.0

local PITCH_LIMIT = math.pi * 0.5 - 0.08

local function norm(x, y, z)
  local l = math.sqrt(x * x + y * y + z * z)
  if l < 1e-6 then return 0, 0, 0, 0 end
  return x / l, y / l, z / l, l
end

local function cross(ax, ay, az, bx, by, bz)
  return ay * bz - az * by, az * bx - ax * bz, ax * by - ay * bx
end

local ship
local was_piloting = false

local function acquire()
  for _, s in ipairs(findScripts("planet_walker")) do
    if net.isMine(s.node) then return s.node end
  end
  local w = findScript("planet_walker")
  return (w and w.node) or find("Astronaut") or find("Player")
end

function lateUpdate(node, dt)
  -- While flying, the SHIP is the subject (wider orbit); on exit, snap back.
  -- Swap on the TRANSITION, not by handle comparison (handles are fresh
  -- tables per access — equality never matches, which stuck the camera on
  -- the ship after exit).
  if not ship then ship = findScript("ship_controller") end
  local piloting = (ship and ship.piloting) or false
  if piloting ~= was_piloting then
    was_piloting = piloting
    target = piloting and ship.node or acquire()
  end
  if not (target and target.valid) then
    target = acquire()
    if not target then return end
  end
  if pitch == nil then pitch = params.start_pitch end

  -- SHIFT is ship throttle while piloting — don't fight over it.
  if input.pressed("shift") and not piloting then shiftlock = not shiftlock end

  params.distance = params.distance - input.scroll() * params.zoom_speed
  local maxd = piloting and math.max(params.max_distance, 40.0) or params.max_distance
  -- On foot you can scroll all the way IN: first person (the astronaut hides
  -- so you don't sit inside the capsule). Flying keeps a minimum orbit.
  local mind = piloting and params.min_distance or 0.0
  if params.distance > maxd then params.distance = maxd end
  if params.distance < mind then params.distance = mind end

  -- Local up from the body (−gravity). Fallback: away from the origin (the
  -- planet sits at 0,0,0) if the body state isn't available yet.
  local ux, uy, uz = target.up_x, target.up_y, target.up_z
  if not ux or (ux == 0 and uy == 0 and uz == 0) then
    ux, uy, uz = norm(target.x, target.y, target.z)
    if ux == 0 and uy == 0 and uz == 0 then ux, uy, uz = 0, 1, 0 end
  end

  -- Parallel-transport the yaw reference: project the previous reference onto
  -- the new tangent plane. Walking around the planet turns the frame WITH the
  -- surface, so the camera never rolls wildly or flips at the equator.
  local d = rx * ux + ry * uy + rz * uz
  local l
  rx, ry, rz, l = norm(rx - ux * d, ry - uy * d, rz - uz * d)
  if l < 1e-4 then
    -- Degenerate (reference was parallel to up): pick any tangent.
    rx, ry, rz = norm(cross(ux, uy, uz, 1, 0, 0))
    if rx == 0 and ry == 0 and rz == 0 then
      rx, ry, rz = norm(cross(ux, uy, uz, 0, 0, 1))
    end
  end

  -- Mouse steers while looking (RMB / shift lock); yaw rotates the reference
  -- around up (Rodrigues, u ⊥ r so it's just cos/sin), pitch is clamped.
  local looking = shiftlock or input.button(1)
  input.setMouseLocked(looking)
  if looking then
    local dx, dy = input.mouse_delta()
    local s = params.sensitivity * 0.01
    local a = -dx * s
    local ca, sa = math.cos(a), math.sin(a)
    local cx, cy, cz = cross(ux, uy, uz, rx, ry, rz)
    rx, ry, rz = norm(rx * ca + cx * sa, ry * ca + cy * sa, rz * ca + cz * sa)
    pitch = pitch - dy * s
    if pitch > PITCH_LIMIT then pitch = PITCH_LIMIT end
    if pitch < -PITCH_LIMIT then pitch = -PITCH_LIMIT end
  end

  -- View direction in the local frame: reference tilted by pitch toward up.
  local cp, sp = math.cos(pitch), math.sin(pitch)
  local fx = rx * cp + ux * sp
  local fy = ry * cp + uy * sp
  local fz = rz * cp + uz * sp

  -- Look-at point: the character's head (along LOCAL up, not world Y).
  local hx = target.x + ux * params.height
  local hy = target.y + uy * params.height
  local hz = target.z + uz * params.height

  -- Wall clip: cast from the head back toward the camera, ignore the player.
  local back = params.distance
  local hit = raycast(hx, hy, hz, -fx, -fy, -fz, params.distance + 0.3, target)
  if hit and hit.distance then
    back = math.max(mind * 0.5, hit.distance - 0.3)
  end

  -- First person on foot: with the camera at the head, hide the body so you
  -- aren't looking at the inside of your own capsule. (While piloting the
  -- ship script owns the astronaut's visibility — leave it alone.)
  if not piloting then
    target.visible = back >= 0.7
  end

  local px = hx - fx * back
  local py = hy - fy * back
  local pz = hz - fz * back
  node.x, node.y, node.z = px, py, pz

  -- Orient the camera node: engine Euler order is YXZ (yaw, pitch, roll).
  -- yaw/pitch place the forward; roll aligns the camera's up with the LOCAL
  -- up (projected ⊥ forward) so the horizon reads level on the planet.
  local yaw2 = math.atan2(-fx, -fz)
  local pit2 = math.asin(math.max(-1, math.min(1, fy)))
  -- Desired camera up: local up made perpendicular to the view direction.
  local du = ux * fx + uy * fy + uz * fz
  local wx, wy, wz = norm(ux - fx * du, uy - fy * du, uz - fz * du)
  if wx == 0 and wy == 0 and wz == 0 then wx, wy, wz = 0, 1, 0 end
  -- Undo yaw (about Y) then pitch (about X) to read the roll left over.
  local cy2, sy2 = math.cos(-yaw2), math.sin(-yaw2)
  local ax = wx * cy2 + wz * sy2
  local ay = wy
  local az = -wx * sy2 + wz * cy2
  local cx2, sx2 = math.cos(-pit2), math.sin(-pit2)
  local by = ay * cx2 - az * sx2
  local bz2 = ay * sx2 + az * cx2
  local _ = bz2
  node.yaw, node.pitch, node.roll = yaw2, pit2, math.atan2(-ax, by)

  -- Publish the basis for the walker + dig tool.
  fwd_x, fwd_y, fwd_z = fx, fy, fz
  cam_x, cam_y, cam_z = px, py, pz
  local fd = fx * ux + fy * uy + fz * uz
  flat_x, flat_y, flat_z = norm(fx - ux * fd, fy - uy * fd, fz - uz * fd)
  if flat_x == 0 and flat_y == 0 and flat_z == 0 then
    flat_x, flat_y, flat_z = rx, ry, rz
  end
end
