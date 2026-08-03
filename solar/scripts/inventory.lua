-- INVENTORY — every container in the game: the suit you carry ore home in, the
-- cargo hold you fly it back in, and the warehouse it gets sold out of.
--
-- A container is a bare id and a bag of counts. Three kinds exist today:
--
--   "astro"        the astronaut's suit pack — mass-limited, this is the one
--                  that makes a full load a DECISION
--   "hold:<ship>"  a vessel's cargo holds, pooled (capacity = its Cargo Bay
--                  parts; the vessel registers it when it spawns)
--   "base"         the home warehouse — effectively unlimited, sells from here
--
-- MASS is the only limit. Every container has a kilogram capacity, materials
-- weigh what `materials.lua` says they weigh, and `add` is allowed to accept
-- PART of an offer — a tool that mines 6 units into a pack with room for 2 puts
-- 2 in and tells you it dropped 4. Silently voiding the rest, or letting the
-- pack go over, are both worse than an honest partial.
--
-- All state lives in `save.*`: containers survive the builder → system scene
-- hop, ride the save slot, and are what a "bring it home" loop is actually
-- measured against.
--
--     local inv = findScript("inventory")
--     inv.add("astro", "iron", 6)          -- → units actually taken
--     inv.transferAll("astro", "hold:Kestrel I")
--     inv.mass("astro"), inv.cap("astro")

defaults = {
  suit_kg = 45.0,     -- what an astronaut can carry in the suit pack
  base_kg = 1000000,  -- the home warehouse: a number, but not one you'll hit
}

local function mats()
  return findScript("materials")
end

local function kg_of(mat)
  local m = mats()
  return (m and m.kg(mat)) or 1.0
end

local function bag(cid)
  return save.get("inv." .. cid) or {}
end

local function put(cid, b)
  -- Empty slots are removed rather than kept at zero: a bag is a small save
  -- value (≤1 KB), and zero rows would pad both the file and every panel.
  local out = {}
  for k, v in pairs(b) do
    if (v or 0) > 0 then out[k] = math.floor(v) end
  end
  -- An emptied bag is written as `{}`, not nil: the save store takes VALUES
  -- (numbers, strings, bools, small tables) — the rest of the project writes
  -- `false`/`{}` for "cleared" and nothing deletes a key.
  save.set("inv." .. cid, out)
end

-- ── capacity ────────────────────────────────────────────────────────────────
-- Suit and warehouse are fixed by the params; a vessel registers its hold when
-- it spawns (the sum of its Cargo Bay parts), so capacity follows the craft you
-- actually built rather than a number kept in two places.

function setCap(cid, kilos)
  local caps = save.get("inv.cap") or {}
  caps[cid] = math.max(0, kilos or 0)
  save.set("inv.cap", caps)
end

function cap(cid)
  if cid == "astro" then return params.suit_kg end
  if cid == "base" then return params.base_kg end
  local caps = save.get("inv.cap") or {}
  return caps[cid] or 0
end

function mass(cid)
  local total = 0
  for mat, n in pairs(bag(cid)) do total = total + kg_of(mat) * n end
  return total
end

function free(cid)
  return math.max(0, cap(cid) - mass(cid))
end

-- ── contents ────────────────────────────────────────────────────────────────

function count(cid, mat)
  return bag(cid)[mat] or 0
end

function isEmpty(cid)
  return next(bag(cid)) == nil
end

-- Contents in the registry's stable order, with the numbers a panel wants.
function items(cid)
  local b, m = bag(cid), mats()
  local out = {}
  local order = m and m.list() or nil
  if order then
    for _, mat in ipairs(order) do
      local n = b[mat]
      if n and n > 0 then
        out[#out + 1] = { mat = mat, n = n, kg = kg_of(mat) * n,
                          value = (m and m.valueOf(mat, n)) or 0 }
      end
    end
  else
    for mat, n in pairs(b) do
      out[#out + 1] = { mat = mat, n = n, kg = kg_of(mat) * n, value = 0 }
    end
    table.sort(out, function(a, c) return a.mat < c.mat end)
  end
  return out
end

function totalValue(cid)
  local m = mats()
  if not m then return 0 end
  local v = 0
  for mat, n in pairs(bag(cid)) do v = v + m.valueOf(mat, n) end
  return math.floor(v)
end

-- ── moving things ───────────────────────────────────────────────────────────

-- How many units of `mat` would fit right now. Zero-mass materials would divide
-- by nothing, so they're treated as weightless-but-finite (the offer, whole).
function roomFor(cid, mat, want)
  local k = kg_of(mat)
  if k <= 0 then return want or 0 end
  return math.min(want or 0, math.floor(free(cid) / k + 1e-9))
end

-- Add up to `n` units. Returns how many actually went in — callers are expected
-- to use it (a mining tool reports the shortfall; a transfer leaves the rest
-- where it was).
function add(cid, mat, n)
  n = math.floor(n or 0)
  if n <= 0 or not mat then return 0 end
  local take = roomFor(cid, mat, n)
  if take <= 0 then return 0 end
  local b = bag(cid)
  b[mat] = (b[mat] or 0) + take
  put(cid, b)
  -- First sight of a material anywhere is a discovery; research reads this set.
  local seen = save.get("inv.seen") or {}
  if not seen[mat] then
    seen[mat] = true
    save.set("inv.seen", seen)
    local m = mats()
    log("NEW MATERIAL: " .. ((m and m.name(mat)) or mat))
  end
  return take
end

-- Remove up to `n`. Returns how many came out.
function remove(cid, mat, n)
  n = math.floor(n or 0)
  if n <= 0 then return 0 end
  local b = bag(cid)
  local have = b[mat] or 0
  local took = math.min(have, n)
  if took <= 0 then return 0 end
  b[mat] = have - took
  put(cid, b)
  return took
end

-- Move what fits. Nothing is destroyed in transit: whatever the destination
-- can't take stays in the source.
function transfer(from, to, mat, n)
  n = math.min(math.floor(n or 0), count(from, mat))
  if n <= 0 then return 0 end
  local moved = add(to, mat, n)
  if moved > 0 then remove(from, mat, moved) end
  return moved
end

function transferAll(from, to)
  local moved = 0
  for _, it in ipairs(items(from)) do
    moved = moved + transfer(from, to, it.mat, it.n)
  end
  return moved
end

function clear(cid)
  save.set("inv." .. cid, {})
end

-- Have we ever held this material? (The discovery set — research + the depot's
-- "first sale" bonus read it.)
function seen(mat)
  local s = save.get("inv.seen") or {}
  return s[mat] == true
end

function seenList()
  local s = save.get("inv.seen") or {}
  local m = mats()
  local out = {}
  for _, id in ipairs(m and m.list() or {}) do
    if s[id] then out[#out + 1] = id end
  end
  return out
end

-- A one-line summary for HUDs: "12.4 / 45 kg".
function line(cid)
  return string.format("%.1f / %.0f kg", mass(cid), cap(cid))
end
