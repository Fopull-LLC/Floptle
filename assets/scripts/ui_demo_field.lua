-- The call-sign field. The value IS the element's text, so there is nothing to
-- keep in sync: read `node.text`.

function changed(node)
    local demo = findScript("ui_demo")
    if demo then
        local n = #node.text
        demo:say(n == 0 and "type a call sign" or ("call sign: " .. node.text))
    end
end

function submitted(node)
    local demo = findScript("ui_demo")
    if demo then demo:say("registered " .. node.text .. " — Enter is `submitted`, not `clicked`") end
end
