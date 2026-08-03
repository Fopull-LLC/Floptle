-- One CONTRACT row on the Ops Board. `slot` 1 is always the contract on the
-- books (if any); the rest are the jobs you could sign, in catalogue order.
-- Clicking signs — or, on the active row, abandons.
--
-- Rows with nothing to show hide themselves, so the board's stack closes up
-- rather than leaving a column of empty boxes.

defaults = { slot = 1 }

local el, entry, is_active

local function missions()
  return findScript("missions")
end

function start(node)
  el = node:getcomponent("UiElement")
end

function update(node, dt)
  if not el then el = node:getcomponent("UiElement") end
  if not el then return end
  local mi = missions()
  if not mi then
    el.visible = false
    return
  end
  local slot = math.floor(params.slot)
  local active = mi.active and mi.byId(mi.active) or nil
  entry, is_active = nil, false
  if active then
    if slot == 1 then
      entry, is_active = active, true
    else
      entry = mi.offers()[slot - 1]
    end
  else
    entry = mi.offers()[slot]
  end
  el.visible = entry ~= nil
  if not entry then return end
  local co = findScript("company")
  local money = (co and co.money) and co.money(entry.reward)
    or ("$" .. tostring(entry.reward))
  local text = string.format("  %s%-22s %s   rep %+d",
    is_active and "◆ " or "  ", entry.title, money, entry.rep or 0)
  if node.text ~= text then node.text = text end
  if is_active then
    el.fillR, el.fillG, el.fillB, el.fillA = 0.10, 0.28, 0.20, 0.94
    el.textR, el.textG, el.textB = 0.66, 1.0, 0.80
  else
    el.fillR, el.fillG, el.fillB, el.fillA = 0.10, 0.14, 0.18, 0.9
    el.textR, el.textG, el.textB = 0.72, 0.86, 0.96
  end
end

function clicked(node)
  local mi = missions()
  if not mi or not entry then return end
  if is_active then
    mi.abandon()
  else
    mi.accept(entry.id)
  end
end

function hoverStart(node)
  if el then el.opacity = 0.85; el.border = 1.6 end
  -- Hovering a contract shows its brief on the board's own hint line: the
  -- catalogue stays scannable, and the detail is one mouse-move away.
  local h = find("Ops Hint")
  if h and entry then h.text = "  " .. (entry.brief or ""):gsub("\n", "  ") end
end

function hoverEnd(node)
  if el then el.opacity = 1.0; el.border = 1.0 end
end
