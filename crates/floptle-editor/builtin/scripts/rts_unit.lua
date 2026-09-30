-- An RTS unit: takes a move order and finds its own way there.
--
-- SETUP — the whole thing, in order:
--
--   1. Build a level. Anything a unit should not walk through needs the
--      **Collidable** switch on it: that is what a navmesh bake looks for, and
--      it is the same switch physics uses, so a wall that stops a crate stops a
--      unit without being tagged twice.
--   2. ✚ Add ▸ ⬚ Nav Mesh. Set radius/height to the size of a unit, then press
--      **Bake**. The Scene view draws the walkable surface; check it looks like
--      the floor you can actually stand on, and that the Inspector does not say
--      "N separate areas" where you expected one.
--   3. Attach this script to anything commandable — a capsule, a model, a
--      prefab — and rts_commander.lua to one node that has the camera.
--
-- The bake is saved beside the scene and loaded with it. You do not bake again
-- unless the level changes; if you are still building it, tick "bake again when
-- the level changes" on the Nav Mesh node and it will keep itself current on
-- another thread.
--
-- Pairs with rts_commander.lua, which does the selecting and the ordering:
-- left-click / drag a box to select, right-click the ground to send them. This
-- script is the half you'll replace first — it is deliberately small, and every
-- hook a real game wants is a named function on it:
--
--   moveTo(x, y, z)   walk to a WORLD point (what an order calls)
--   stop()            cancel the current order, here
--   isMoving()        true while it still has somewhere to be
--   isBlocked()       true when it gave up: unreachable, or shoved and stuck
--   remaining()       metres left ALONG THE ROUTE, not through the walls
--   selected          true while the player has it selected (draws the ring)
--
-- Attacking, gathering, build queues and formations all hang off `moveTo` +
-- `isMoving` — give the unit a state variable and act on arrival.
--
-- WITH NO NAVMESH it still works: it walks straight at the order, which is what
-- this script did before pathfinding existed. That is deliberate — adding a
-- Nav Mesh node should be the thing that makes units clever, not the thing that
-- makes an existing scene stop working.

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
  --@header Crowd
  -- How wide this unit is when it comes to going round its neighbours. Separate
  -- from the navmesh's own radius, which is about walls — so a tank and a
  -- scout can share one bake and still not stand inside each other.
  --@range 0.1 5 --@units m
  radius = 0.6,
  -- Steer around other units. Off makes this one walk its line and let the
  -- others sort themselves out — right for a boss or a scripted march.
  avoid = true,
  --@header Debug
  -- Draw the route this unit chose, while it is selected.
  show_path = false,
  --@header Selection ring
  -- Drawn on the ground while selected — immediate-mode lines, no extra nodes.
  ring = true,
  --@range 0.2 10 --@units m
  ring_radius = 1.1,
}

-- Public state — the commander reads and writes these through a script handle.
selected = false

-- The nav agent, once there is a navmesh to give it. `nil` means no bake in
-- this scene, and everything below falls back to walking straight at the order.
local agent
-- The straight-line fallback's state, used only when there is no navmesh.
local tx, tz = 0.0, 0.0
local has_order = false
local best, stalled = math.huge, 0.0

function start(node)
  if nav.ready() then
    agent = nav.agent(node, {
      speed = params.speed,
      accel = params.accel,
      arrive = params.arrive,
      radius = params.radius,
      avoid = params.avoid,
      giveUpAfter = params.give_up_after,
      -- `auto` steers a physics body through the body and moves a plain node's
      -- transform, which is what makes this work on a capsule with a Rigidbody
      -- and on a bare shape without one.
      drive = "auto",
    })
  end
end

--- Order the unit to a world point. `y` is ignored — units walk the ground.
function moveTo(x, _y, z)
  if agent then
    agent:moveTo(vec3(x, _y or 0, z))
    return
  end
  tx, tz = x, z
  has_order = true
  best, stalled = math.huge, 0.0
end

--- Cancel the order and stand still.
function stop()
  if agent then
    agent:stop()
    return
  end
  has_order = false
end

function isMoving()
  if agent then return agent.moving end
  return has_order
end

--- True once it has given up: the goal is unreachable, or it has been stuck.
--- Worth acting on — a unit that stops for a reason should say the reason.
function isBlocked()
  return agent ~= nil and agent.blocked
end

--- How far it still has to walk, along the route rather than through the walls.
function remaining()
  if agent then return agent.remaining end
  return 0.0
end

-- A unit ordered somewhere the navmesh does not describe is a LEVEL problem
-- wearing a unit's clothes: it looks exactly like one that cannot get there,
-- and no order will fix it. Said once, because it is true every frame until
-- somebody grows the Nav Mesh box and bakes again.
local warned_off_mesh = false
local function watch_off_mesh(node)
  if not agent or not agent.offMesh or warned_off_mesh then return end
  warned_off_mesh = true
  log(
    node.name .. " was ordered somewhere the navmesh does not cover. Grow the Nav Mesh " ..
    "node's box (or tick 'fit the box to what it finds') and bake again."
  )
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

-- The route it picked, drawn on the ground. The fastest way to answer "why did
-- it go THAT way" — usually because the way you expected is not walkable.
local function path_at(node)
  local from = node.worldPos
  for _, c in ipairs(agent:corners()) do
    draw.line(from.x, from.y + 0.1, from.z, c.x, c.y + 0.1, c.z, 0.35, 0.9, 1.0)
    from = c
  end
end

function update(node, dt)
  watch_off_mesh(node)
  if params.ring and selected then
    -- Red while it is stuck, so a unit that stopped for a reason says so
    -- without anybody opening the console.
    if isBlocked() then
      ring_at(node, 1.0, 0.4, 0.35)
    else
      ring_at(node, 0.35, 1.0, 0.5)
    end
  end

  if agent then
    -- The engine walks the crowd once a frame, after every script's update, so
    -- there is nothing to step here. Facing is still ours: the agent decides
    -- where to go, the game decides what that should look like.
    if params.show_path and selected then path_at(node) end
    if params.turn_speed > 0 then
      local v = agent.velocity
      if v:length() > 0.05 then node:turnTowards(v, params.turn_speed * dt) end
    end
    return
  end

  -- ---- no navmesh in this scene: walk straight at it ----------------------
  --
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
