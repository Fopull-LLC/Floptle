-- Lag-compensated melee swing (pairs with parry_dummy.lua on a target).
--
-- SETUP: attach to your character (the node with third_person.lua + a
-- Networked component). LEFT CLICK swings at whatever you're looking at.
--
--   offline Play        → judged immediately, no networking needed
--   Test as remote      → the click ships to the hidden server stamped with
--                         the tick you were SEEING; the server rewinds and
--                         judges there (docs/scripting.md §15). Crank the
--                         latency slider: a parry that was up on YOUR screen
--                         still counts.

defaults = {
  reach = 4.5,
  damage = 25,
  eye = 1.0,        -- the swing ray starts this far above the node origin
  cooldown = 0.35,
}

local next_swing = 0

-- The view direction, same math as third_person_camera.lua's forward.
local function aimDir()
  local yaw = input.aimYaw() or node.yaw
  local pitch = input.aimPitch() or 0
  local cp = math.cos(pitch)
  return -math.sin(yaw) * cp, math.sin(pitch), -math.cos(yaw) * cp
end

-- Judge one swing on the authoritative side. peer = who swung (0 = offline).
local function judge(ox, oy, oz, dx, dy, dz, peer)
  local hit = raycast(ox, oy, oz, dx, dy, dz, params.reach)
  if not hit then
    log("swing: miss")
    return
  end
  if hit.node then
    local dummy = hit.node:getscript("parry_dummy")
    if dummy then
      if dummy.synced.parrying then
        log("swing: PARRIED by " .. hit.node.name)
        if net.isServer() then
          net.rpc("parried", { x = hit.x, y = hit.y, z = hit.z }, { to = peer })
        else
          spawnEffect("vfx/Hit", hit.x, hit.y, hit.z)
        end
        return
      end
      dummy.hurt(params.damage, dx, dz)
      log("swing: hit " .. hit.node.name .. " for " .. params.damage)
      return
    end
    log("swing: clang — hit " .. hit.node.name)
  else
    log("swing: hit the world")
  end
end

function update(node, dt)
  if input.clicked(0) and time >= next_swing then
    next_swing = time + params.cooldown
    local dx, dy, dz = aimDir()
    local ox, oy, oz = node.x, node.y + params.eye, node.z
    if net.isClient() then
      -- ship the intent stamped with what I was seeing; the server judges
      net.rpc("swing", { ox = ox, oy = oy, oz = oz, dx = dx, dy = dy, dz = dz },
              { withInput = true })
    elseif net.role() == "offline" then
      judge(ox, oy, oz, dx, dy, dz, 0)
    end
    -- (on a server the replayed click does nothing — swings arrive as rpcs)
  end
end

onRpc = {}

function onRpc.swing(args, peer)
  if not net.isServer() then return end
  net.rewind(peer, function()
    judge(args.ox, args.oy, args.oz, args.dx, args.dy, args.dz, peer)
  end)
end

-- back on the attacker's screen: your hit was blocked fair and square
function onRpc.parried(args)
  log("PARRIED — the window was up on YOUR screen, so it counts")
  spawnEffect("vfx/Hit", args.x, args.y, args.z)
end
