-- A cargo slot.
--
-- The engine reports the drag and MOVES NOTHING — so what a drop does is one
-- decision, made here. This slot moves the label between two crates that were
-- always in the scene; a different game would tween the source across, or
-- re-parent it, or fire off a particle. None of those is more "correct", which
-- is exactly why the engine doesn't pick one.

local function crate(node)
    local kids = node:children()
    return kids[1]
end

function dropped(node)
    local item = ui.dragging()
    local mine = crate(node)
    local demo = findScript("ui_demo")
    if not item or not mine then return end
    -- Already holding something? Say so rather than silently swallowing it.
    if mine:getcomponent("UiElement").visible then
        if demo then demo:say(node.name .. " is full") end
        return
    end
    mine.text = item.text
    mine:getcomponent("UiElement").visible = true
    item:getcomponent("UiElement").visible = false
    if demo then demo:say("moved " .. mine.text .. " into " .. node.name) end
end

function dragEnter(node)
    local demo = findScript("ui_demo")
    if demo then demo:say("over " .. node.name) end
end
