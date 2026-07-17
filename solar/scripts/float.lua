-- float.lua — bob up and down and slowly spin.
--
-- Shows real per-instance game logic: start() runs once and stashes state
-- (the start height + a random phase) so multiple copies don't move in lockstep,
-- and update() reads it every frame.

defaults = { height = 0.5, speed = 1.0, spin = 20 }

function start(node)
  base_y = node.y                 -- remembered per instance
  phase = math.random() * math.pi * 2
end

function update(node, dt)
  node.y = base_y + math.sin(time * params.speed + phase) * params.height
  node.yaw = node.yaw + math.rad(params.spin) * dt
end
