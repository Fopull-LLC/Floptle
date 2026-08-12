-- Scene Report — a small, complete Floptle editor extension.
--
-- Everything a package is likely to reach for appears here once: a panel, a
-- menu item, a shortcut, a Scene-view overlay, world-space handles, reading the
-- scene, editing it with undo, and remembering a setting between sessions.
--
-- Reference: docs/editor-scripting.md

-- Settings, remembered per person across every project.
local markBig = ed.prefs.get("markBig", true)
local bigRadius = ed.prefs.get("bigRadius", 4.0)

-- ---------------------------------------------------------------------------
-- Reading the scene
-- ---------------------------------------------------------------------------

-- Count nodes by kind, and collect the ones bigger than the threshold.
-- `scene.info` reads this frame's mirror, so this is cheap enough to run while
-- a panel is drawing.
local function survey()
    local counts, big, total = {}, {}, 0
    for _, id in ipairs(scene.all()) do
        local n = scene.info(id)
        if n then
            total = total + 1
            counts[n.kind] = (counts[n.kind] or 0) + 1
            if n.radius and n.radius >= bigRadius then
                big[#big + 1] = { id = id, name = n.name, radius = n.radius }
            end
        end
    end
    -- Sorted so the panel does not reshuffle itself every frame.
    local kinds = {}
    for kind in pairs(counts) do kinds[#kinds + 1] = kind end
    table.sort(kinds)
    table.sort(big, function(a, b) return a.radius > b.radius end)
    return counts, kinds, big, total
end

-- ---------------------------------------------------------------------------
-- The panel
-- ---------------------------------------------------------------------------

local panel = ed.window("Scene Report", function()
    local project = ed.project()
    gui.heading(project.scene ~= "" and project.scene or "(no scene)")

    local counts, kinds, big, total = survey()
    gui.label(total .. " node" .. (total == 1 and "" or "s"))
    gui.separator()

    gui.scroll(function()
        for _, kind in ipairs(kinds) do
            gui.horizontal(function()
                gui.label(kind)
                gui.label(tostring(counts[kind]))
            end)
        end
    end)

    gui.separator()
    local wasMark = markBig
    markBig = gui.checkbox(markBig, "mark the big ones in the Scene view")
    bigRadius = gui.slider(bigRadius, 0.5, 50, "bigger than")
    if markBig ~= wasMark then
        ed.prefs.set("markBig", markBig)
    end
    ed.prefs.set("bigRadius", bigRadius)

    gui.label(#big .. " over the threshold")
    if #big > 0 and gui.button("Select them", "replaces the current selection") then
        local ids = {}
        for _, b in ipairs(big) do ids[#ids + 1] = b.id end
        selection.set(ids)
    end

    -- An edit, with a real undo point: Ctrl+Z after this puts the names back.
    local sel = selection.get()
    gui.enabled(#sel > 0, function()
        if gui.button("Tag selection as 'big'", "renames each selected node") then
            ed.undo()
            for _, id in ipairs(sel) do
                local n = scene.info(id)
                if n and not n.name:find("%[big%]") then
                    scene.setName(id, n.name .. " [big]")
                end
            end
        end
    end)
end)

-- ---------------------------------------------------------------------------
-- Getting to it
-- ---------------------------------------------------------------------------

ed.menu("Scene Report/Report…", function() panel:show() end)
ed.menu("Scene Report/What is selected", function()
    local sel = selection.get()
    if #sel == 0 then
        ed.message("Scene Report", "Nothing is selected.")
        return
    end
    local lines = {}
    for _, id in ipairs(sel) do
        local n = scene.info(id)
        if n then lines[#lines + 1] = n.name .. "  (" .. n.kind .. ")" end
    end
    ed.message("Selected", table.concat(lines, "\n"))
end)

ed.shortcut("Ctrl+R", function() panel:toggle() end)

-- ---------------------------------------------------------------------------
-- Drawing in the world
-- ---------------------------------------------------------------------------

ed.onSceneDraw(function()
    if not markBig then return end
    local _, _, big = survey()
    handles.color(1.0, 0.65, 0.2, 0.9)
    for _, b in ipairs(big) do
        local n = scene.info(b.id)
        if n then
            handles.wireSphere(n.worldPos, b.radius)
            handles.label(n.worldPos, n.name)
        end
    end
    -- The selection gets a box, in a second colour.
    handles.color(0.4, 0.85, 1.0)
    for _, id in ipairs(selection.get()) do
        local bounds = scene.bounds(id)
        if bounds and bounds.radius then
            handles.wireCube(bounds.center, vec3(
                (bounds.max.x - bounds.min.x),
                (bounds.max.y - bounds.min.y),
                (bounds.max.z - bounds.min.z)
            ))
        end
    end
end)

-- A Scene-view overlay: pinned in the viewport, always to hand.
ed.overlay("Scene Report", function()
    local _, _, big, total = survey()
    gui.small(total .. " nodes · " .. #big .. " big")
    if gui.smallButton("open") then panel:show() end
end)

-- ---------------------------------------------------------------------------
-- Hooks
-- ---------------------------------------------------------------------------

ed.onSceneOpen(function()
    ed.log("scene opened: " .. ed.project().scene)
end)
