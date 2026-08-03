-- An RTS unit: takes a move order and walks there.
--
-- SETUP: attach to anything you want commandable — a capsule, a model, a
-- prefab. It drives a Rigidbody if the node has one (so units collide, ride
-- slopes and can't walk through walls) and falls back to moving the transform
-- directly when it doesn't, so it also works on a plain shape with no physics.
--
-- Pairs with rts_commander.lua, which does the selecting and the ordering:
-- left-click / drag a box to select, right-click the ground to send them. This
-- script is the half you'll replace first — it is deliberately small, and every
-- hook a real game wants is a named function on it:
--
--   moveTo(x, y, z)   walk to a WORLD point (what an order calls)
--   stop()            cancel the current order, here
--   isMoving()        true while it still has somewhere to be
--   selected          true while the player has it selected (draws the ring)
--
-- Orders are world-space and the unit measures itself with `node.worldX/Z`, so
-- a unit parented under a container arrives where you clicked. (`node.x` is
-- LOCAL — comparing that against a world target is how a click-to-move unit
-- walks past its destination and keeps going forever.)
--
-- Attacking, gathering, build queues and formations all hang off `moveTo` +
-- `isMoving` — give the unit a state variable and act on arrival.

defaults = {
  --@header Movement
  --@range 0 40 --@units m/s
  speed = 7.0,
  -- How hard it gets up to speed (and back down). High = crisp and arcadey,
  -- low = heavy vehicles with visible momentum.
  --@range 1 60
  accel = 24.0,
  -- Close enough to the order to call it done. Keep it at least the unit's
  -- radius or a crowd will jostle forever trying to stand on one spot.
  --@range 0.1 10 --@units m
  arrive = 0.8,
  -- Give up if the unit hasn't got measurably closer for this long — walked
  -- into a wall, ordered somewhere it can't reach, shoved off course. Without
  -- it a blocked unit pushes forever. 0 disables the guard.
  --@range 0 30 --@units s
  give_up_after = 3.0,
  -- Turn rate toward the way it's travelling. 0 leaves the facing alone (for
  -- units whose model is spun by an animation or a turret script instead).
  --@range 0 30 --@units rad/s
  turn_speed = 9.0,
  --@header Selection ring
  -- Drawn on the ground while selected — immediate-mode lines, no extra nodes.
  ring = true,
  --@range 0.2 10 --@units m
  ring_radius = 1.1,
}

-- Public state — the commander reads and writes these through a script handle.
selected = false

local tx, tz = 0.0, 0.0
local has_order = false
-- Progress watchdog: the closest we have been to this order, and how long ago.
local best, stalled = math.huge, 0.0

--- Order the unit to a world point. `y` is ignored — units walk the ground.
function moveTo(x, _y, z)
  tx, tz = x, z
  has_order = true
  best, stalled = math.huge, 0.0
end

--- Cancel the order and stand still.
function stop()
  has_order = false
end

function isMoving()
  return has_order
end

local function ring_at(node, r, g, b)
  local cx0, cy0, cz0 = node.worldX, node.worldY, node.worldZ
  local y = cy0 - params.ring_radius * 0.5 + 0.05
  local n = 20
  local px, pz = cx0 + params.ring_radius, cz0
  for i = 1, n do
    local a = (i / n) * math.pi * 2
    local cx = cx0 + math.cos(a) * params.ring_radius
    local cz = cz0 + math.sin(a) * params.ring_radius
    draw.line(px, y, pz, cx, y, cz, r, g, b)
    px, pz = cx, cz
  end
end

function update(node, dt)
  if params.ring and selected then ring_at(node, 0.35, 1.0, 0.5) end

  -- Horizontal offset to the order, in WORLD space (RTS movement is a
  -- ground-plane problem — gravity or the ground itself owns the vertical).
  local want = vec3(0, 0, 0)
  if has_order then
    local goal = vec3(tx, node.worldY, tz)
    local left = node:distanceFlat(goal)
    if left <= params.arrive then
      has_order = false -- arrived
    else
      -- Watchdog: only "making progress" counts as being closer than ever.
      if left < best - 0.05 then
        best, stalled = left, 0.0
      else
        stalled = stalled + dt
        if params.give_up_after > 0 and stalled > params.give_up_after then
          has_order = false -- blocked or unreachable: stop instead of shoving
        end
      end
      if has_order then
        want = dirTo(node.worldPos, goal):flatten() * params.speed
      end
    end
  end

  local body = node:getcomponent("RigidBody")
  if body then
    -- Physics unit: steer the body's own velocity and leave the vertical to
    -- the sim, so units stay on the ground, collide, and ride slopes. `ease` is
    -- the frame-rate-independent approach — the same at 30 fps and 240.
    node.vel = ease(node.vel:withY(0), want, params.accel, dt):withY(node.vel.y)
  else
    node.pos = node.pos + want * dt
  end

  -- Face where it is going (never snap a facing from a standstill). One call,
  -- and it can't get the sign or the ±pi seam wrong.
  if params.turn_speed > 0 and want:length() > 0 then
    node:turnTowards(want, params.turn_speed * dt)
  end
end
