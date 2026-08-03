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
--   moveTo(x, y, z)   walk to a world point (what an order calls)
--   stop()            cancel the current order, here
--   isMoving()        true while it still has somewhere to be
--   selected          true while the player has it selected (draws the ring)
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

local tx, ty, tz = 0.0, 0.0, 0.0
local has_order = false

--- Order the unit to a world point. `y` is optional (it walks the ground).
function moveTo(x, y, z)
  tx, ty, tz = x, y or 0.0, z
  has_order = true
end

--- Cancel the order and stand still.
function stop()
  has_order = false
end

function isMoving()
  return has_order
end

local function ring_at(node, r, g, b)
  local y = node.y - params.ring_radius * 0.5 + 0.05
  local n = 20
  local px, pz = node.x + params.ring_radius, node.z
  for i = 1, n do
    local a = (i / n) * math.pi * 2
    local cx = node.x + math.cos(a) * params.ring_radius
    local cz = node.z + math.sin(a) * params.ring_radius
    draw.line(px, y, pz, cx, y, cz, r, g, b)
    px, pz = cx, cz
  end
end

function update(node, dt)
  if params.ring and selected then ring_at(node, 0.35, 1.0, 0.5) end

  -- Horizontal offset to the order (RTS movement is a ground-plane problem —
  -- gravity or the ground itself owns the vertical).
  local dx, dz = 0.0, 0.0
  if has_order then
    dx, dz = tx - node.x, tz - node.z
    if math.sqrt(dx * dx + dz * dz) <= params.arrive then
      has_order, dx, dz = false, 0.0, 0.0
    end
  end
  local want_x, want_z = 0.0, 0.0
  if has_order then
    local len = math.sqrt(dx * dx + dz * dz)
    want_x, want_z = dx / len * params.speed, dz / len * params.speed
  end

  local body = node:getcomponent("RigidBody")
  if body then
    -- Physics unit: steer the body's own velocity and leave the vertical to
    -- the sim, so units stay on the ground, collide, and ride slopes.
    local k = 1.0 - math.exp(-params.accel * dt) -- frame-rate-independent approach
    node.vx = node.vx + (want_x - node.vx) * k
    node.vz = node.vz + (want_z - node.vz) * k
  else
    node.x = node.x + want_x * dt
    node.z = node.z + want_z * dt
  end

  -- Face where it is going (never snap a facing from a standstill).
  if params.turn_speed > 0 and (want_x ~= 0 or want_z ~= 0) then
    local want_yaw = math.atan2(-want_x, -want_z) -- engine forward is −Z (atan2: LuaJIT/5.1)
    local d = (want_yaw - node.yaw + math.pi) % (math.pi * 2) - math.pi
    node.yaw = node.yaw + d * math.min(1.0, params.turn_speed * dt)
  end
end
