-- float.lua — bob up and down and slowly spin.
--
-- Shows real per-instance game logic: start() runs once and stashes state
-- (the start height + a random phase) so multiple copies don't move in lockstep,
-- and update() reads it every frame.

defaults = {
  --@range 0 5 --@units m
  height = 0.5,
  --@range 0 10 --@units Hz
  speed = 1.0,
  --@range -360 360 --@units deg/s
  spin = 20,
}

-- File-scope locals, not globals: each copy of the script gets its own, and
-- nothing else in the project can read or clobber them by accident.
local base_y, phase

function start(node)
  base_y = node.y                 -- remembered per instance
  phase = math.random() * math.pi * 2
end

function update(node, dt)
  node.y = base_y + math.sin(time * params.speed + phase) * params.height
  node.yaw = node.yaw + math.rad(params.spin) * dt
end
