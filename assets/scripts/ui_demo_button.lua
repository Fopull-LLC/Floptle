-- A demo button. `says` is a string param, so the same script serves every
-- button in the scene and the Inspector decides what each one reports.
defaults = { says = "clicked" }

-- Submit on a gamepad fires this SAME hook a mouse click fires — a button
-- written for a pointer works with a pad and there is no second code path.
function clicked(node)
    local demo = findScript("ui_demo")
    if demo then demo:say(params.says .. " — a pad submit and a mouse click are one hook") end
end

function cancelled(node)
    local demo = findScript("ui_demo")
    if demo then demo:say("cancelled") end
end
