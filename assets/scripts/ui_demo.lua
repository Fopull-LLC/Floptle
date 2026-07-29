-- The UI demo's one script.
--
-- Read it as the answer to "how much Lua does a screen like this need". The
-- answer is this file: no per-frame layout arithmetic, no hover functions, no
-- list rebuilding, no key polling. Everything visual is in the scene and the
-- style sheet; this only holds the state and says what depends on it.

manifest = {
    "Iron ore",
    "Refined fuel",
    "Hull plating",
    "Nav computer",
    "Ration crate",
    "Coolant loop",
    "Spare thruster",
    "Survey drone",
    "Ice core sample",
    "Distress beacon",
}

status = "focus with the arrows or a d-pad · submit with Enter or A"

function start(node)
    -- The list. One binding replaces a spawn loop, a destroy loop and the
    -- bookkeeping that keeps them agreeing.
    ui.bind(find("Manifest List"), "count", function() return #manifest end)

    -- The status line. Says the relationship once; the engine keeps it true.
    ui.bind(find("Status"), "text", function() return status end)

    -- A colour that follows state. `color(...)` is one value, not four
    -- channels, so a conditional colour is one expression.
    ui.bind(find("Status"), "textColor", function()
        if status:find("dropped") or status:find("launched") then
            return color.hex("#66de8f")
        end
        return color(0.56, 0.60, 0.68)
    end)

    -- Open with something focused, so a pad player can start without
    -- touching the mouse.
    ui.focus(find("Play"))
end

-- Called by the row / slot / button scripts.
function say(msg)
    status = msg
end

function take(index)
    local name = manifest[index + 1]
    if not name then return end
    table.remove(manifest, index + 1)
    status = "removed " .. name .. " — the list kept every other row's state"
end
