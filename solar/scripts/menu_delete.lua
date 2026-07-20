-- The ✕ button beside a save slot: deletes that save FROM DISK — both stores,
-- paired (save.deleteSlot wipes the key→value file, terrain.deleteSaveDir the
-- slot's persisted terrain) — so the slot is immediately reusable. Two-click
-- confirm: the first click arms it ("sure?", red); it disarms by itself.
-- Hidden while the slot is empty (nothing to delete).

defaults = { slot = 1 }

local el, me
local armed_until = 0

local function peek(name, key)
  local cur = save.slot()
  save.slot(name)
  local v = save.get(key)
  save.slot(cur)
  return v
end

local function slot_name()
  return "slot" .. math.floor(params.slot)
end

local function idle_fill()
  if el then
    el.fillR, el.fillG, el.fillB, el.fillA = 0.10, 0.14, 0.18, 0.88
  end
end

local function refresh(node)
  local occupied = peek(slot_name(), "g_seed") ~= nil
  if el then el.visible = occupied end
  node.text = "✕"
  armed_until = 0
  idle_fill()
end

function start(node)
  el = node:getcomponent("UiElement")
  me = node
  refresh(node)
end

function clicked(node)
  if time < armed_until then
    -- Confirmed: wipe both stores, relabel the slot button, hide ourselves.
    save.deleteSlot(slot_name())
    terrain.deleteSaveDir("saves/" .. slot_name() .. "/terrain")
    local btn = find("Slot " .. math.floor(params.slot))
    if btn then
      local s = btn:getscript("menu_slot")
      if s then s.relabel() end
    end
    refresh(node)
    print(string.format("save %s deleted", slot_name()))
  else
    armed_until = time + 3.0
    node.text = "sure?"
    if el then
      el.fillR, el.fillG, el.fillB, el.fillA = 0.5, 0.12, 0.12, 0.95
    end
  end
end

function update(node, dt)
  if armed_until > 0 and time >= armed_until then
    armed_until = 0
    node.text = "✕"
    idle_fill()
  end
end

function hoverStart(node)
  if el and time >= armed_until then
    el.fillR, el.fillG, el.fillB, el.fillA = 0.32, 0.16, 0.2, 0.95
  end
end

function hoverEnd(node)
  if time >= armed_until then idle_fill() end
end
