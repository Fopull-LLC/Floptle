-- Third-person orbit camera (pairs with third_person.lua).
--
-- SETUP: attach to a Camera node and mark it Active. It finds the character
-- automatically (the node running third_person.lua — or a node named "Player").
--
--   RIGHT MOUSE (hold)  orbit around the character
--   SCROLL WHEEL        zoom in / out
--   SHIFT               toggle SHIFT LOCK: the cursor locks, the mouse steers
--                       the camera, and the character faces the camera's yaw
--   zoom all the way in → FIRST PERSON: the camera sits at head height, the
--   character model hides, you free-look (always shift-locked); scroll back
--   out to return.
--
-- The camera raycasts against the world so walls never cut between you and
-- the character (it slides in closer instead of clipping through geometry).
--
-- Zoom lives in `params.distance` — params are TWO-WAY: the script's writes
-- persist, update live in the Inspector, and other scripts can read them.
-- Other scripts can also read this script's state through a handle:
--
--   local cam = findScript("third_person_camera")
--   if cam and cam.firstPerson then ... end   -- e.g. show a first-person HUD
--   if cam and cam.shiftlock  then ... end

defaults = {
  distance = 6.0,       -- orbit distance (scroll zooms; Inspector-live)
  min_distance = 1.2,   -- zooming closer than this switches to first person
  max_distance = 14.0,
  height = 1.4,         -- look-at point above the character's origin
  sensitivity = 0.3,
  zoom_speed = 1.0,
  start_pitch = -0.35,  -- initial down-tilt (radians)
}

-- Exposed state — env globals, NOT locals, so other scripts (the character
-- controller's shift-lock facing, a first-person HUD) read them via a handle.
firstPerson = false
shiftlock = false

local target        -- the character node we orbit
local model         -- its "Model" child (hidden in first person)

local PITCH_LIMIT = math.pi * 0.5 - 0.05

-- The character to follow: of every third_person controller in the scene,
-- the one THIS machine controls (net.isMine — in multiplayer each player's
-- camera picks their own avatar; offline everything is "mine" so the first
-- controller wins, exactly as before).
local function acquire()
  for _, s in ipairs(findScripts("third_person")) do
    if net.isMine(s.node) then return s.node end
  end
  local tp = findScript("third_person")           -- spectator fallback
  return (tp and tp.node) or find("Player")
end

function start(node)
  target = acquire()
  if target then model = target:find("Model") end
  node.pitch = params.start_pitch
end

-- lateUpdate, not update: cameras run AFTER physics and the interpolated
-- transform writeback, so the follow reads the character's FINAL pose this
-- frame. In `update` it would read last frame's pose — a velocity × dt lag
-- that turns frame-time noise into visible movement jitter.
function lateUpdate(node, dt)
  -- Re-acquire when the target dies (despawn) or stops being ours (joining a
  -- session re-assigns avatars once the server says who we are).
  if not (target and target.valid) or not net.isMine(target) then
    target = acquire()
    if target then model = target:find("Model") end
  end
  if not (target and target.valid) then return end

  -- SHIFT toggles shift lock; first person forces it below.
  if input.pressed("shift") then shiftlock = not shiftlock end

  -- Scroll zooms. `params.distance` is two-way: the write persists across
  -- frames and shows live in the Inspector. Crossing min_distance toggles
  -- first person.
  params.distance = params.distance - input.scroll() * params.zoom_speed
  if params.distance > params.max_distance then params.distance = params.max_distance end
  if params.distance < 0.0 then params.distance = 0.0 end
  firstPerson = params.distance < params.min_distance

  -- The camera looks (and the cursor captures) while: first person, shift
  -- lock, or the RIGHT mouse button is dragging. Otherwise the cursor is
  -- free and the camera holds its angle.
  local looking = firstPerson or shiftlock or input.button(1)
  input.setMouseLocked(looking)
  if looking then
    local dx, dy = input.mouse_delta()
    local s = params.sensitivity * 0.01
    node.yaw = node.yaw - dx * s
    node.pitch = node.pitch - dy * s
    if node.pitch > PITCH_LIMIT then node.pitch = PITCH_LIMIT end
    if node.pitch < -PITCH_LIMIT then node.pitch = -PITCH_LIMIT end
  end

  -- Show/hide the character model when the view mode flips.
  if model and model.valid then
    model.visible = not firstPerson
  end

  -- The point we look at / stand in: the character's head.
  local hx = target.x
  local hy = target.y + params.height
  local hz = target.z

  if firstPerson then
    -- First person: sit at head height and free-look.
    node.x, node.y, node.z = hx, hy, hz
    return
  end

  -- Orbit: back away from the head along the view direction (yaw/pitch),
  -- engine forward = −Z. fy uses sin(pitch) so looking up raises the view.
  local cp = math.cos(node.pitch)
  local fx = -math.sin(node.yaw) * cp
  local fy = math.sin(node.pitch)
  local fz = -math.cos(node.yaw) * cp

  -- Don't clip through walls: cast from the head back toward the camera.
  -- `target` rides along as the ignore — rays hit physics bodies too, and the
  -- character's own capsule must never count as an obstruction.
  local back = params.distance
  local hit = raycast(hx, hy, hz, -fx, -fy, -fz, params.distance + 0.3, target)
  if hit and hit.distance then
    back = math.max(params.min_distance, hit.distance - 0.3)
  end

  node.x = hx - fx * back
  node.y = hy - fy * back
  node.z = hz - fz * back
end
