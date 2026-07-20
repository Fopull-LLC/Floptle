
-- One script, many portals: each instance sets its own destination
-- in the Inspector (a string default = a text field).
defaults = { destination = "untitled" }

function onTriggerEnter(node, other, hit)
  if other:hasTag("player") then
    scene.load(params.destination)
  end
end
