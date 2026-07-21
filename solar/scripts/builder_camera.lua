-- BUILDER CAMERA — free-fly, the anti-KSP (no vertical-axis jail, no gimbal
-- surprises). You think about the craft, not the camera.
--
--   RMB (hold)   mouselook
--   W/A/S/D      fly (camera-relative)   Q/E  down / up
--   SHIFT        fly faster
--   wheel        dolly along the view
--   F            focus the ship: swings to frame it; RMB then ORBITS the ship
--                center until any fly key breaks back to free flight
--   HOME         return to the start pose
--
-- The builder script reads our published `focusing` to keep placement clicks
-- and camera drags from fighting.

defaults = {
  speed = 8.0,        -- fly speed, units/s (SHIFT ×3)
  look = 0.0032,      -- mouselook radians per pixel
  wheel = 1.2,        -- dolly units per wheel notch
}

-- Published for the builder script.
focusing = false     -- true while in focus-orbit mode

local HOME = { x = 0.0, y = 3.2, z = 10.0, yaw = 0.0, pitch = -0.12 }

local yaw, pitch = HOME.yaw, HOME.pitch
local orbit_dist = 8.0
local builder

local function fwd_right()
  local cy, sy = math.cos(yaw), math.sin(yaw)
  local cp, sp = math.cos(pitch), math.sin(pitch)
  -- -Z forward at yaw 0.
  local f = vec3(-sy * cp, sp, -cy * cp)
  local r = vec3(cy, 0, -sy)
  return f, r
end

local function ship_center()
  if not builder then builder = findScript("builder") end
  if builder and builder.centerX then
    return vec3(builder.centerX, builder.centerY, builder.centerZ), builder.partCount or 0
  end
  return vec3(0, 1.5, 0), 0
end

function start(node)
  node.x, node.y, node.z = HOME.x, HOME.y, HOME.z
  node.yaw, node.pitch = yaw, pitch
end

function update(node, dt)
  local rmb = input.button(1)
  if rmb then input.lockMouse() else input.unlockMouse() end

  -- Mouselook / orbit.
  if rmb then
    local dx, dy = input.mouse_delta()
    yaw = yaw - dx * params.look
    pitch = pitch - dy * params.look
    if pitch > 1.5 then pitch = 1.5 end
    if pitch < -1.5 then pitch = -1.5 end
  end

  -- Fly input (breaks focus-orbit back to free flight).
  local f, r = fwd_right()
  local move = vec3(0, 0, 0)
  local flying = false
  local function key(k, v)
    if input.key(k) then move = move + v; flying = true end
  end
  key("w", f); key("s", vec3(-f.x, -f.y, -f.z))
  key("d", r); key("a", vec3(-r.x, -r.y, -r.z))
  key("e", vec3(0, 1, 0)); key("q", vec3(0, -1, 0))
  if flying then focusing = false end

  -- F = focus the ship (also useful on an empty pad: frames the work floor).
  if input.pressed("f") then
    focusing = true
    local c, n = ship_center()
    orbit_dist = math.max(6.0, 2.5 + n * 1.1)
    -- Look at the center from our current bearing.
    -- Aim from the camera AT the center: forward = (c - cam)/d, and with
    -- f = (-sin(yaw)cos(p), sin(p), -cos(yaw)cos(p)) that solves to:
    local dx, dy, dz = node.x - c.x, node.y - c.y, node.z - c.z
    local d = math.sqrt(dx * dx + dy * dy + dz * dz)
    if d > 0.01 then
      yaw = math.atan2(dx, dz)
      pitch = math.asin(-dy / d)
    end
  end
  if input.pressed("home") then
    focusing = false
    node.x, node.y, node.z = HOME.x, HOME.y, HOME.z
    yaw, pitch = HOME.yaw, HOME.pitch
  end

  local speed = params.speed * (input.key("shift") and 3.0 or 1.0)
  if focusing then
    -- Orbit: sit at orbit_dist from the ship center along our view ray.
    local c = ship_center()
    orbit_dist = math.max(2.0, orbit_dist - input.scroll() * params.wheel)
    local f2 = fwd_right()
    node.x = c.x - f2.x * orbit_dist
    node.y = c.y - f2.y * orbit_dist
    node.z = c.z - f2.z * orbit_dist
  else
    local dolly = input.scroll() * params.wheel
    node.x = node.x + (move.x * speed * dt) + f.x * dolly
    node.y = node.y + (move.y * speed * dt) + f.y * dolly
    node.z = node.z + (move.z * speed * dt) + f.z * dolly
    if node.y < 0.4 then node.y = 0.4 end -- soft floor: never under the deck
  end

  node.yaw, node.pitch, node.roll = yaw, pitch, 0
end
