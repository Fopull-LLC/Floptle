-- script.lua
--
-- `defaults` are tunables shown in the Inspector; `params` are this
-- instance's live values. `node` is the node's transform (x/y/z,
-- scale/scale_x..z, yaw/pitch/roll in radians). `time` = seconds since
-- play started, `dt` = frame delta. The full Lua stdlib is in scope.

defaults = { speed = 1.0 }

function start(node)
  -- runs once when play begins
end

function update(node, dt)

end

function fixedUpdate(node, dt)
  node.yaw = node.yaw + math.rad(90) * dt   -- rotates identically at any fps
end