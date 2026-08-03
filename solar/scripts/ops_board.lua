-- OPS BOARD — the contracts panel at the Command Centre, and the one screen
-- that tells you what the company is actually doing: balance, standing, the
-- contract on the books with its goals ticked off, and every job you could
-- sign next.
--
-- Opened from the Command Centre's facility panel. Rows are `ops_row` — one per
-- offered contract, plus the active one at the top; clicking signs (or, for the
-- active one, abandons). Rows past the end of the list hide themselves so the
-- stack collapses.

local el, title, hint_n

function start(node)
  el = node:getcomponent("UiElement")
  if el then el.visible = false end
end

function isOpen()
  if not el then el = (find("Ops Board") or {}).getcomponent
    and find("Ops Board"):getcomponent("UiElement") end
  return el and el.visible or false
end

function openBoard()
  if not el then return end
  el.visible = true
end

function closeBoard()
  if el then el.visible = false end
end

function toggle()
  if not el then return end
  el.visible = not el.visible
end

function update(node, dt)
  if not el then el = node:getcomponent("UiElement") end
  if not el or not el.visible then return end
  -- Walking away closes it: the board is a thing you stand at, and a panel that
  -- follows you across the map is a panel you fight.
  local fm = findScript("facility_menu")
  local astro = find("Astronaut")
  if not astro or astro.visible == false then
    el.visible = false
    return
  end
  if not title then title = find("Ops Title") end
  if not hint_n then hint_n = find("Ops Hint") end
  local co = findScript("company")
  local mi = findScript("missions")
  if title then
    local head = "OPS BOARD"
    if co and co.money then
      head = string.format("OPS BOARD\n%s        reputation %+d",
        co.money(co.balance()), co.rep())
    end
    if mi and mi.active then
      local lines = mi.goalLines()
      if #lines > 0 then
        head = head .. "\n\nON THE BOOKS — " .. (mi.activeLine() or "")
        for _, l in ipairs(lines) do head = head .. "\n   " .. l end
      end
    end
    if title.text ~= head then title.text = head end
  end
  if hint_n then
    local n = mi and #mi.offers() or 0
    local txt
    if mi and mi.active then
      txt = "  click the contract on the books to abandon it (costs standing)"
    elseif n > 0 then
      txt = "  click a contract to sign it   ·   E or Esc to leave"
    else
      txt = "  no contracts available — complete the ones you have"
    end
    if hint_n.text ~= txt then hint_n.text = txt end
  end
  if input.pressed("escape") then el.visible = false end
end
