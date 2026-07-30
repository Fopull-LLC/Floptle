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

-- The crew. Nothing in the scene file knows how many of these there are, what
-- they are called, or that they exist — the whole right-hand panel is built
-- from this table by `ui.make` below.
crew = {
    { id = "vex",   name = "Vex Arroyo",   role = "pilot" },
    { id = "juno",  name = "Juno Park",    role = "engineer" },
    { id = "sable", name = "Sable Nkemi",  role = "navigator" },
    { id = "orin",  name = "Orin Vale",    role = "medic" },
    { id = "reya",  name = "Reya Fossk",   role = "gunner" },
}
offDuty = {}

-- One crew row: a portrait badge, a name, a role, and what clicking it does.
-- Structure and behaviour in the same expression — the row needs no prefab,
-- no script file and no entry in the scene.
local function crewRow(member)
    return {
        "button", key = member.id, style = "row",
        w = "100%", h = 44, dir = "row", gap = 10, pad = 8, align = "center",
        tooltip = member.name .. " — click to stand them down",
        onClicked = function() standDown(member.id) end,
        {
            "box", w = 26, h = 26, radius = 13, fill = color(0.18, 0.40, 0.52),
            text = member.name:sub(1, 1), textAlign = "center", textSize = 13,
        },
        { "col", gap = 0, pad = 0, w = "grow",
            { "text", text = member.name, textSize = 15 },
            { "text", text = member.role, textSize = 12, textColor = color(0.56, 0.60, 0.68) },
        },
    }
end

-- The panel. Called whenever the data changes, NOT every frame: the engine
-- spawns and destroys only the difference, so the rows that stay keep their
-- hover, their in-flight transitions and their entity.
function buildCrew()
    ui.make(find("Crew Panel"), {
        "col", inset = 0, style = "panel", gap = 10, pad = 16,
        { "text", w = "100%", h = 18, style = "caption",
          text = "CREW  ·  " .. #crew .. " on duty" },
        { "col", w = "100%", gap = 6, pad = 0, items = crew, crewRow },
        -- A function child with no `items` is a conditional part of the
        -- screen: return nil and it simply isn't there.
        function()
            if #offDuty == 0 then return nil end
            return {
                "button", style = "button/ghost", w = "100%", h = 32,
                text = "RECALL " .. #offDuty, textAlign = "center",
                onClicked = recall,
            }
        end,
    })
end

function standDown(id)
    for i, m in ipairs(crew) do
        if m.id == id then
            table.insert(offDuty, table.remove(crew, i))
            status = m.name .. " stood down — the other rows kept their state"
            buildCrew()
            return
        end
    end
end

function recall()
    for _, m in ipairs(offDuty) do table.insert(crew, m) end
    offDuty = {}
    status = "whole crew recalled — one table, one ui.make, five rows back"
    buildCrew()
end

function start(node)
    -- The right-hand panel, described as data.
    buildCrew()

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
