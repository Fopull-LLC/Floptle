-- Third-person orbit camera (pairs with third_person.lua).
--
-- SETUP: attach to a Camera node and mark it Active. It finds the character
-- automatically (the node running third_person.lua — or a node named "Player").
--
--   MOUSE          orbit around the character (the cursor is captured)
--   SCROLL WHEEL   zoom in / out
--   zoom all the way in → FIRST PERSON: the camera sits at head height, the
--   character model hides, and you free-look; scroll back out to return.
--
-- The camera raycasts against the world so walls never cut between you and
-- the character (it slides in closer instead of clipping through geometry).

defaults = {
  distance = 6.0,       -- current orbit distance (zoom changes it)
  min_distance = 1.2,   -- zooming closer than this switches to first person
  max_distance = 14.0,
  height = 1.4,         -- look-at point above the character's origin
  sensitivity = 0.3,
  zoom_speed = 1.0,
  start_pitch = -0.35,  -- initial down-tilt (radians)
}

local target        -- the character node we orbit
local model         -- its "Model" child (hidden in first person)
local dist
local first_person = false

local PITCH_LIMIT = math.pi * 0.5 - 0.05

function start(node)
  local tp = findScript("third_person")
  target = (tp and tp.node) or find("Player")
  if target then model = target:find("Model") end
  dist = params.distance
  node.pitch = params.start_pitch
end

function update(node, dt)
  if not (target and target.valid) then return end

  local dx, dy = input.mouse_delta()
  local s = params.sensitivity * 0.01
  node.yaw = node.yaw - dx * s
  node.pitch = node.pitch - dy * s
  if node.pitch > PITCH_LIMIT then node.pitch = PITCH_LIMIT end
  if node.pitch < -PITCH_LIMIT then node.pitch = -PITCH_LIMIT end

  -- Scroll zooms; crossing min_distance toggles first person.
  dist = dist - input.scroll() * params.zoom_speed
  if dist > params.max_distance then dist = params.max_distance end
  if dist < 0.0 then dist = 0.0 end
  first_person = dist < params.min_distance

  -- Show/hide the character model when the view mode flips.
  if model and model.valid then
    model.visible = not first_person
  end

  -- The point we look at / stand in: the character's head.
  local hx = target.x
  local hy = target.y + params.height
  local hz = target.z

  if first_person then
    input.setMouseLocked(true)
    -- First person: sit at head height and free-look.
    node.x, node.y, node.z = hx, hy, hz
    return
   else
     input.setMouseLocked(false)
  end

  -- Orbit: back away from the head along the view direction (yaw/pitch),
  -- engine forward = −Z. fy uses sin(pitch) so looking up raises the view.
  local cp = math.cos(node.pitch)
  local fx = -math.sin(node.yaw) * cp
  local fy = math.sin(node.pitch)
  local fz = -math.cos(node.yaw) * cp

  -- Don't clip through walls: cast from the head back toward the camera.
  local back = dist
  local hit = raycast(hx, hy, hz, -fx, -fy, -fz, dist + 0.3)
  if hit and hit.distance then
    back = math.max(params.min_distance, hit.distance - 0.3)
  end

  node.x = hx - fx * back
  node.y = hy - fy * back
  node.z = hz - fz * back
end
