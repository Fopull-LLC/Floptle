-- rotate.lua — spin a node around its Y (up) axis.
--
-- `defaults` are the tunables the Inspector shows; `params` are this instance's
-- live values. `node.yaw` is the heading in radians; `dt` is the frame delta.

defaults = { speed = 45 }  -- degrees per second

function update(node, dt)
  node.yaw = node.yaw + math.rad(params.speed) * dt
end
