-- Spawn one avatar per joining player (the real multiplayer shape — no
-- authored slot per player needed).
--
-- SETUP: attach to any always-present node (the Map, or an empty
-- "GameManager"). Each joiner gets scenes/player.ron spawned FOR them: the
-- engine registers its physics body live, the joiner's machine binds
-- prediction to it (it responds instantly at any latency), and everyone else
-- sees it interpolated. When a player disconnects, their avatar despawns
-- automatically.
--
-- The scene's own Predicted node (if any) stays the HOST's avatar (slot #1).

defaults = {
  spawn_x = 0.0,
  spawn_y = 2.5,
  spawn_z = 8.0,
  spread = 2.0,       -- joiners fan out along +x so they don't stack
}

function start(node)
  net.on("playerJoined", function(peer)
    if net.isServer() then
      net.spawn("scenes/player.ron", {
        x = params.spawn_x + peer * params.spread,
        y = params.spawn_y,
        z = params.spawn_z,
        owner = peer,
      })
      log("spawned an avatar for peer " .. peer)
    end
  end)
end
