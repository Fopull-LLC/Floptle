-- One row of the demo's manifest list.
--
-- The whole interface between "there are ten of these" and "this one is the
-- third" is `node.index`. Nothing here knows the list's length, and nothing
-- in the list knows what a row says.

function update(node, dt)
    local demo = findScript("ui_demo")
    if not demo or node.index == nil then return end
    local name = demo.manifest[node.index + 1]
    node.text = name and ("  " .. string.format("%02d", node.index + 1) .. "   " .. name) or ""
end

-- Clicking a row removes it, which is the interesting case: the OTHER rows
-- keep their scripts' state, their hover, their in-flight style transitions
-- and the view's scroll position, because the engine spawns and destroys only
-- the difference rather than rebuilding the list.
function clicked(node)
    local demo = findScript("ui_demo")
    if demo and node.index ~= nil then demo:take(node.index) end
end
