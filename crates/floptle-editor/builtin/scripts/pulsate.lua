-- pulsate.lua — breathe a node's scale with a sine wave.
--
-- `time` is seconds since play started. `node.scale` sets a uniform scale (there
-- are also node.scale_x / scale_y / scale_z for per-axis control).

defaults = {
  --@range 0 2
  amplitude = 0.3,
  --@range 0 20 --@units Hz
  speed = 2.0,
  --@range 0.01 10
  base = 1.0,
}

function update(node, dt)
  local f = params.base * (1.0 + params.amplitude * math.sin(params.speed * time))
  node.scale = math.max(f, 0.01)
end
