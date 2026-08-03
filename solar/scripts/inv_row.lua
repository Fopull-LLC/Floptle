-- One MATERIAL row in the inventory panel. `slot` indexes the pack's contents
-- in the registry's order; rows past the end hide themselves so the panel's
-- stack closes up instead of leaving empty boxes.
--
-- Clicking moves that whole stack to whatever the panel has in reach (a craft's
-- hold, or the warehouse). With nothing in reach the row is inert — and says so
-- through the panel's hint line rather than a popup.

defaults = { slot = 1 }

local el, entry

local function panel()
  return findScript("inventory_panel")
end

function start(node)
  el = node:getcomponent("UiElement")
end

function update(node, dt)
  if not el then el = node:getcomponent("UiElement") end
  if not el then return end
  local inv, mats = findScript("inventory"), findScript("materials")
  if not (inv and mats) then
    el.visible = false
    return
  end
  local items = inv.items("astro")
  entry = items[math.floor(params.slot)]
  el.visible = entry ~= nil
  if not entry then return end
  local co = findScript("company")
  local money = (co and co.money) and co.money(entry.value) or ("$" .. entry.value)
  local text = string.format("  %4d × %-18s %6.1f kg %8s",
    entry.n, mats.name(entry.mat), entry.kg, money)
  if node.text ~= text then node.text = text end
  -- Tier tints the row: the drab stuff reads drab, the good stuff doesn't.
  local t = mats.tier(entry.mat)
  if t >= 3 then
    el.textR, el.textG, el.textB = 1.0, 0.86, 0.55
  elseif t == 2 then
    el.textR, el.textG, el.textB = 0.72, 0.96, 0.80
  else
    el.textR, el.textG, el.textB = 0.78, 0.86, 0.94
  end
end

function clicked(node)
  local p = panel()
  if p and entry and p.moveStack then p.moveStack(entry.mat) end
end

function hoverStart(node)
  if el then el.opacity = 0.85; el.border = 1.6 end
end

function hoverEnd(node)
  if el then el.opacity = 1.0; el.border = 1.0 end
end
