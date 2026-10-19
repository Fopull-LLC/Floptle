-- ui.engineSettings(container, page [, opts]) — the engine's own Controls and
-- Accessibility pages, built into a game's container with ui.make.
--
-- The structure and the behaviour are the engine's; the look is the game's.
-- Nothing here paints: every element takes its style from opts.styles, so the
-- page wears the game's theme, and with no styles it is plain text and
-- buttons. A rebind needs one poll a frame (the key the player presses arrives
-- between frames), so the page hands back a handle whose :update() the
-- game's own update calls while the page is open.

local PAGES = { controls = true, accessibility = true }
local KEYS = { styles = true, onChanged = true, filter = true }
local STYLE_KEYS = { page = true, row = true, label = true, value = true, button = true }

local function check(opts)
  for k in pairs(opts) do
    if not KEYS[k] then
      error("ui.engineSettings: no option `" .. tostring(k) .. "` — it takes styles, onChanged, filter", 3)
    end
  end
  for k in pairs(opts.styles or {}) do
    if not STYLE_KEYS[k] then
      error("ui.engineSettings: no style slot `" .. tostring(k) .. "` — it takes page, row, label, value, button", 3)
    end
  end
end

function ui.engineSettings(container, page, opts)
  if not PAGES[page] then
    error('ui.engineSettings: the page is "controls" or "accessibility", not ' .. tostring(page), 2)
  end
  opts = opts or {}
  check(opts)
  local st = opts.styles or {}
  local changed = opts.onChanged or function() end
  local panel = { page = page, waiting = nil }

  local function row(key, label, value, onClicked)
    return {
      "row", key = key, style = st.row,
      { "text", key = "label", text = label, style = st.label },
      { "button", key = "value", style = st.button, onClicked = onClicked,
        { "text", key = "text", text = value, style = st.value } },
    }
  end

  local function controls()
    local rows = { "col", key = "controls", style = st.page }
    for _, action in ipairs(input.actions()) do
      local chips = input.bindingsOf(action)
      local value
      if panel.waiting == action then
        value = "press a key..."
      elseif #chips > 0 then
        value = table.concat(chips, ", ")
      else
        value = "unbound"
      end
      rows[#rows + 1] = row(action, action, value, function()
        input.startRebind(action, opts.filter)
        panel.waiting = action
        panel:refresh()
      end)
    end
    return rows
  end

  local function accessibility()
    local filters = access.filters()
    local current = access.colorFilter()
    local label = current
    for i, f in ipairs(filters) do
      if f.name == current then
        label = f.label
      end
    end
    local function onoff(b) return b and "on" or "off" end
    return {
      "col", key = "accessibility", style = st.page,
      row("textScale", "Text size", string.format("%d%%", math.floor(access.textScale() * 100 + 0.5)), function()
        -- 100 → 125 → 150 → 200 → 75 → 100: a button a pad can cycle, with
        -- no slider for a player to drag past the end of.
        local steps = { 0.75, 1.0, 1.25, 1.5, 2.0 }
        local now, nxt = access.textScale(), steps[1]
        for _, s in ipairs(steps) do
          if s > now + 0.01 then nxt = s break end
        end
        access.setTextScale(nxt)
        changed("textScale", nxt)
        panel:refresh()
      end),
      row("colorFilter", "Colour filter", label, function()
        local at = 1
        for i, f in ipairs(filters) do
          if f.name == current then at = i end
        end
        local name = filters[at % #filters + 1].name
        access.setColorFilter(name)
        changed("colorFilter", name)
        panel:refresh()
      end),
      row("reducedMotion", "Reduce motion", onoff(access.reducedMotion()), function()
        local on = not access.reducedMotion()
        access.setReducedMotion(on)
        changed("reducedMotion", on)
        panel:refresh()
      end),
      row("captions", "Captions", onoff(access.captions()), function()
        local on = not access.captions()
        access.setCaptions(on)
        changed("captions", on)
        panel:refresh()
      end),
    }
  end

  function panel:refresh()
    ui.make(container, page == "controls" and controls() or accessibility())
  end

  -- Call every frame while the page is open: a captured key is committed and
  -- the row shows it; Escape (or a cancel) puts the row back.
  function panel:update()
    if not self.waiting then return end
    local p = input.pendingRebind()
    if p == nil then
      self.waiting = nil
      self:refresh()
    elseif p ~= "" then
      local action = self.waiting
      input.commitRebind()
      self.waiting = nil
      changed("binding", action)
      self:refresh()
    end
  end

  panel:refresh()
  return panel
end
