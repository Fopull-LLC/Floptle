-- ui.engineSettings(container, page [, opts]) — the engine's own Controls and
-- Accessibility pages, built into a game's container with ui.make.
--
-- The structure and the behaviour are the engine's; the look is the game's.
-- Nothing here paints: every element takes its style from opts.styles, so the
-- page wears the game's theme, and with no styles it is plain text and
-- buttons. A rebind needs one poll a frame (the key the player presses arrives
-- between frames), so the page hands back a handle whose :update() the
-- game's own update calls while the page is open.
--
-- With no container the page builds nothing itself: panel:tree() is its
-- description, for a screen the game describes with ui.make every frame. Built
-- into a container of its own, the page would be taken down by the game's next
-- ui.make of that screen, which does not describe the page's rows.

local PAGES = { controls = true, accessibility = true }
local KEYS = { styles = true, onChanged = true, filter = true }
local STYLE_KEYS = { page = true, row = true, label = true, value = true, button = true }

local function check(opts)
  for k in pairs(opts) do
    if not KEYS[k] then
      error("ui.engineSettings: no option `" .. tostring(k) .. "` — it takes styles, onChanged, filter", 3)
    end
  end
  for k, v in pairs(opts.styles or {}) do
    if not STYLE_KEYS[k] then
      error("ui.engineSettings: no style slot `" .. tostring(k) .. "` — it takes page, row, label, value, button", 3)
    end
    if type(v) ~= "string" and type(v) ~= "table" then
      error("ui.engineSettings: style slot `" .. k .. "` is a style name or a table of ui.make properties, not a " .. type(v), 3)
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

  -- A slot is a style name, or a table of ui.make properties (a style among
  -- them) laid onto that element: sizes, insets, stretch — what a style
  -- cannot say.
  local function dress(el, slot)
    local v = st[slot]
    if type(v) == "string" then
      el.style = v
    elseif type(v) == "table" then
      for k, x in pairs(v) do
        if type(k) == "string" then el[k] = x end
      end
    end
    return el
  end

  local function row(key, label, value, onClicked)
    return dress({
      "row", key = key,
      dress({ "text", key = "label", text = label }, "label"),
      dress({ "button", key = "value", onClicked = onClicked,
        dress({ "text", key = "text", text = value }, "value") }, "button"),
    }, "row")
  end

  -- A chip leads with a device glyph (the editor's fonts carry it; a game's
  -- seldom does, and a missing glyph draws as an empty box). Here a key reads
  -- as its name, and a pad button says it is one.
  local PAD = "\226\151\137" -- the pad chip's glyph
  local function plain(chip)
    local icon, rest = chip:match("^(%S+)%s+(.+)$")
    if icon == PAD then return "Pad " .. rest end
    if icon and icon:byte(1) >= 128 then return rest end
    return chip
  end

  local function controls()
    local rows = dress({ "col", key = "controls" }, "page")
    for _, action in ipairs(input.actions()) do
      local chips = {}
      for i, c in ipairs(input.bindingsOf(action)) do chips[i] = plain(c) end
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
    return dress({
      "col", key = "accessibility",
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
    }, "page")
  end

  -- The page as a ui.make description, for a screen of the game's own.
  function panel:tree()
    return page == "controls" and controls() or accessibility()
  end

  function panel:refresh()
    if container ~= nil then
      ui.make(container, self:tree())
    end
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
