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
local ref = vec3(0, 0, -1)
-- The camera's up, SLEW-LIMITED toward −gravity. On foot it tracks fast (level
-- horizon as you walk); while FLYING it tracks GENTLY, so it follows the planet's
-- local vertical as you fly around the body (no more "upside-down on the far
-- side") yet spreads the discontinuous −gravity flip at an SOI hand-off into an
-- easy reorient instead of a jarring snap — see the up-slew below.
local cup = nil

local PITCH_LIMIT = math.pi * 0.5 - 0.08

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
  -- Map mode (S6 v2): the ship script owns the camera while the 3D map is
  -- open — orbiting the focused body, not the player. Stand down entirely.
  if ship and ship.map_view then return end
  -- While flying, the SHIP is the subject (wider orbit); on exit, snap back.
  -- Swap on the TRANSITION, not by handle comparison (handles are fresh
  -- tables per access — equality never matches, which stuck the camera on
  -- the ship after exit).
  if not ship then ship = findScript("ship_controller") end
  -- Built vessels spawn/despawn at runtime — fetch fresh, never cache, and
  -- scan EVERY instance (several craft can be alive; the piloted one is the
  -- camera's subject, not whichever findScript happens to return first).
  local vessel = nil
  for _, s in ipairs(findScripts("vessel_controller")) do
    if s.piloting then vessel = s break end
  end
  local vpiloting = vessel ~= nil
  local piloting = ((ship and ship.piloting) or false) or vpiloting
  if piloting ~= was_piloting then
    was_piloting = piloting
    target = (vpiloting and vessel.node) or (piloting and ship and ship.node) or acquire()
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
  -- Flying: never zoom INSIDE the hull (the ship visual spans ~2 units).
  local mind = piloting and math.max(params.min_distance, 5.0) or 0.0
  if params.distance > maxd then params.distance = maxd end
  if params.distance < mind then params.distance = mind end

  -- Desired local up from the body (−gravity). Fallback: away from the origin
  -- (the planet sits at 0,0,0) if the body state isn't available yet.
  local want_up = target.up
  if not want_up or want_up:length() == 0 then
    want_up = target.pos:normalized()
    if want_up:length() == 0 then want_up = vec3(0, 1, 0) end
  end
  -- Slew-limit the up so a gravity/SOI transfer can't SNAP the view. Crossing an
  -- SOI boundary flips −gravity to point at a DIFFERENT body (the field is
  -- patched-conic — only the dominant body pulls), a discontinuous jump. FLYING
  -- uses a GENTLE rate: flying around a planet turns the local vertical only
  -- slowly (the orbital rate), so the camera tracks it near-perfectly and never
  -- ends up upside-down on the far side — but the instantaneous SOI flip is spread
  -- over ~a second into a smooth reorient instead of a snap. ON FOOT it tracks
  -- fast so the horizon stays level as you walk around the planet.
  if not cup then cup = want_up end
  if cup:angleTo(want_up) > 1e-4 then
    cup = ease(cup, want_up, piloting and 1.5 or 8.0, dt):normalized()
  end
  local up = cup

  -- Parallel-transport the yaw reference: project the previous reference onto
  -- the new tangent plane. Walking around the planet turns the frame WITH the
  -- surface, so the camera never rolls wildly or flips at the equator.
  ref = ref:flatten(up)
  if ref:length() == 0 then
    -- Degenerate (reference was parallel to up): pick any tangent.
    ref = up:cross(vec3(1, 0, 0)):normalized()
    if ref:length() == 0 then ref = up:cross(vec3(0, 0, 1)):normalized() end
  end

  -- Mouse steers while looking (RMB / shift lock); yaw rotates the reference
  -- around up (Rodrigues, u ⊥ r so it's just cos/sin), pitch is clamped.
  local looking = shiftlock or input.button(1)
  input.setMouseLocked(looking)
  if looking then
    local dx, dy = input.mouse_delta()
    local sens = params.sensitivity * 0.01
    -- Yaw spins the reference about the LOCAL up, not about world +Y — which
    -- is the whole reason this camera exists.
    ref = ref:rotatedAround(up, -dx * sens)
    pitch = math.clamp(pitch - dy * sens, -PITCH_LIMIT, PITCH_LIMIT)
  end

  -- View direction in the local frame: reference tilted by pitch toward up.
  local fwd = (ref * math.cos(pitch) + up * math.sin(pitch)):normalized()

  -- Look-at point: the character's head (along LOCAL up, not world Y). A
  -- piloted VESSEL's center is its CAPSULE, composed HERE from the node's
  -- rendered pose + the published pod-local offset: computed in the camera
  -- pass it sits exactly on the ship this frame draws — a fixedUpdate world
  -- position lags the rails carry (offset + jitter), and a gravity-up height
  -- guess slides off the hull the moment the vessel pitches.
  local head
  if vpiloting and vessel and vessel.podLY and target then
    -- `toWorld` composes the vessel's own rotation for us — the nine-term
    -- YXZ expansion this used to spell out by hand.
    head = target:toWorld(vec3(vessel.podLX or 0, vessel.podLY, vessel.podLZ or 0))
  else
    head = target.pos + up * params.height
  end

  -- Wall clip: cast from the head back toward the camera, ignore the player.
  local back = params.distance
  local hit = raycast(head, -fwd, params.distance + 0.3, target)
  -- The wall clip must see WALLS only: the player's hull and the ship (and the
  -- astronaut parked INSIDE the ship while flying) are not walls — clipping on
  -- them glued the camera to the hull.
  if hit and hit.node then
    local astro = find("Astronaut")
    local shipnode = ship and ship.node
    if (astro and hit.node.id == astro.id)
      or (shipnode and hit.node.id == shipnode.id) then
      hit = nil
    end
  end
  if hit and hit.distance then
    back = math.max(mind * 0.5, hit.distance - 0.3)
  end

  -- First person on foot: with the camera at the head, hide the body so you
  -- aren't looking at the inside of your own capsule. (While piloting the
  -- ship script owns the astronaut's visibility — leave it alone.)
  if not piloting then
    target.visible = back >= 0.7
  end

  local place = head - fwd * back
  -- SCREEN SHAKE: the vessel publishes cam.shake (liftoff rumble, buffeting,
  -- crashes). Jitter the camera position along its right/up so the view rattles
  -- while the look target holds steady. Squared for an ease-in (small = subtle).
  local sh = save.get("cam.shake") or 0.0
  if sh > 0.002 then
    local amp = sh * sh * 0.5
    place = place
      + ref * ((math.random() * 2 - 1) * amp)
      + up * ((math.random() * 2 - 1) * amp)
  end
  node.pos = place

  -- Point the camera down `fwd` with the LOCAL up overhead, so the horizon
  -- reads level all the way around the planet. `lookAt` with an `up` sets the
  -- roll too — that is the entire twenty-line undo-yaw-then-pitch dance.
  node:lookAt(place + fwd, up)

  -- Publish the basis for the walker + dig tool.
  fwd_x, fwd_y, fwd_z = fwd.x, fwd.y, fwd.z
  cam_x, cam_y, cam_z = place.x, place.y, place.z
  local flat = fwd:flatten(up)
  if flat:length() == 0 then flat = ref end
  flat_x, flat_y, flat_z = flat.x, flat.y, flat.z
end
