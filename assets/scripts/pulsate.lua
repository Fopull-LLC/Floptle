-- pulsate.lua — breathe a node's scale with a sine wave.
--
-- `time` is seconds since play started. `node.scale` sets a uniform scale (there
-- are also node.scale_x / scale_y / scale_z for per-axis control).

defaults = { amplitude = 0.3, speed = 2.0, base = 1.0 }

function on_update(node, dt)
  local f = params.base * (1.0 + params.amplitude * math.sin(params.speed * time))
  node.scale = math.max(f, 0.01)
end
