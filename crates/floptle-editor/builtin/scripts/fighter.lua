-- An arcade-fighter controller: walk, jump, normals, and a quarter-circle
-- special — all on named ACTIONS, so it plays identically on a pad and on the
-- keyboard, and both characters run this same file.
--
-- SETUP
--   1. Project Settings → Input → "seed a starter map", then add the actions
--      this file uses: Punch, Kick, Block. Click ＋ on each and press the
--      button you want, once on the keyboard and once on a pad.
--   2. Attach this script to BOTH characters. Set `params.player` to 1 on one
--      and 2 on the other — that is the whole of local versus.
--   3. Give each character a Rigidbody (lock its rotation).
--
-- Everything below runs in `fixedUpdate`, not `update`. Fighting games are
-- built on frame counts, and the fixed tick is the only clock that ticks at the
-- same rate for every player regardless of their monitor.

defaults = {
  player = 1,      -- which local player drives this character (1 or 2)
  speed = 6.0,     -- walk speed
  jump = 9.0,      -- jump impulse
  buffer = 4,      -- ticks of input leniency — 4 is a common starting point
  startup = 3,     -- ticks before an attack becomes active
  active = 3,      -- ticks the attack can hit
  recovery = 8,    -- ticks you're stuck afterwards
}

-- Current move: nil, or { name, tick } counting up from 0.
local move = nil
-- The opponent, resolved once so we can work out which way we're facing.
local foe = nil

local function inRecovery()
  return move ~= nil
end

-- Start an attack if one is buffered and we're free to act. Consuming the
-- press is what stops a 4-tick buffer from firing the move four times.
local function tryAttack(me, name)
  if inRecovery() then return false end
  if not me.buffered(name, params.buffer) then return false end
  me.consume(name, params.buffer)
  move = { name = name, tick = 0 }
  return true
end

function start(node)
  -- Whoever else carries this script is the opponent.
  for _, other in ipairs(findScripts("fighter")) do
    if other.node ~= node then foe = other.node end
  end
end

function fixedUpdate(node, dt)
  local me = input.player(params.player)

  -- Face the opponent, and tell the input layer about it: directions are
  -- mirrored before they reach the motion recogniser, so "quarter-circle
  -- forward" keeps meaning "toward them" after a cross-up. Without this, every
  -- special reverses the instant the characters swap sides.
  local facing = 1
  if foe and foe.valid then
    facing = (foe.x >= node.x) and 1 or -1
    node.yaw = (facing > 0) and (math.pi * 0.5) or (-math.pi * 0.5)
  end
  me.setFacing(facing)

  -- ---- attacks -------------------------------------------------------
  -- The special is checked FIRST: a quarter-circle plus punch must not come
  -- out as a plain punch just because the punch was tested earlier.
  if not inRecovery() then
    if me.motion("qcf") and me.buffered("Punch", params.buffer) then
      me.consume("Punch", params.buffer)
      move = { name = "Fireball", tick = 0 }
      log("qcf + punch!")
    elseif not tryAttack(me, "Punch") then
      tryAttack(me, "Kick")
    end
  end

  if move then
    move.tick = move.tick + 1
    local activeFrom = params.startup
    local activeTo = params.startup + params.active
    if move.tick > activeFrom and move.tick <= activeTo then
      -- Hitbox window. Swap this raycast for whatever your game does.
      local hit = raycast(node.x, node.y, node.z, facing, 0, 0, 1.6, node)
      if hit and hit.node and hit.node ~= node then
        log(move.name .. " connects on " .. hit.node.name)
      end
    end
    if move.tick >= activeTo + params.recovery then
      move = nil
    end
    -- Attacks root you in place, so nothing below runs.
    node.vx, node.vz = 0, 0
    return
  end

  -- ---- movement ------------------------------------------------------
  -- One axis, two devices: this is WASD or the left stick, already deadzoned
  -- and SOCD-resolved (holding ← and → together cancels rather than picking
  -- one at random). Set SOCD to "Last wins" in the Input settings if you'd
  -- rather players be able to pivot with no neutral frame.
  local x, _y = me.axis2("Move")
  local blocking = me.action("Block") and x * facing < 0

  node.vx = blocking and 0 or (x * params.speed)
  if node.grounded and me.buffered("Jump", params.buffer) then
    me.consume("Jump", params.buffer)
    node.vy = params.jump
  end
end
