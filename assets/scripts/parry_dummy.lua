-- Parry dummy: a training target with a cycling parry window.
--
-- SETUP: attach to a capsule with a RigidBody and a Networked component
-- (sync transform + physics). Pairs with sword.lua on your character.
--
-- The window TELEGRAPHS: the dummy BOUNCES while its parry is up (position
-- replicates, so remote players see exactly this). Hit it between bounces
-- and it takes the hit (knockback); hit it while bouncing → parried.

defaults = {
  window = 0.9,      -- seconds the parry window is up
  gap = 1.3,         -- seconds between windows
  bounce = 3.5,      -- telegraph hop speed
  knockback = 10.0,
}

replicated = { parrying = false, hp = 100 }

function fixedUpdate(node, dt)
  if net.isClient() then return end   -- the server owns the dummy
  local cycle = params.window + params.gap
  synced.parrying = (time % cycle) < params.window
  -- telegraph: hop while the window is up
  if synced.parrying and node.grounded then
    node.vy = params.bounce
  end
end

function update(node, dt)
  -- cosmetic swell while parrying (wherever this script runs)
  local target = synced.parrying and 1.25 or 1.0
  node.scale = node.scale + (target - node.scale) * math.min(1, dt * 12)
end

-- called by sword.lua's judge on the authoritative side
function hurt(dmg, dx, dz)
  synced.hp = synced.hp - dmg
  local l = math.sqrt(dx * dx + dz * dz)
  if l > 1e-6 then dx, dz = dx / l, dz / l end
  node.vx = dx * params.knockback
  node.vy = 4.0
  node.vz = dz * params.knockback
  log("dummy: -" .. dmg .. " hp, " .. synced.hp .. " left")
  if synced.hp <= 0 then
    synced.hp = 100
    log("dummy: down! (hp reset to 100)")
  end
end
