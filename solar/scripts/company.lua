-- COMPANY — the ledger the whole game is scored against: funds, reputation,
-- and an audit trail of every credit and debit.
--
-- It is deliberately *only* a ledger. It doesn't know what a rocket is, what a
-- mission is, or where the money came from; it takes an amount and a reason and
-- writes them down. Everything else — the builder charging for a hull, a
-- mission paying out, a recovery refunding what came home — calls in.
--
-- All state lives in `save.*`, so it survives the builder → system scene hop
-- (scripts don't) and rides the save slot (each slot is a separate company).
-- Mount this script on a node in EVERY scene that touches money; whichever copy
-- is loaded reads and writes the same keys.
--
-- Cross-script, the manager pattern:
--     local co = findScript("company")
--     if co.spend(1200, "hull: Kestrel I") then ... end
--     co.balance()   co.rep()   co.ledgerLines(6)

defaults = {
  start_funds = 12000,   -- a fresh company's opening balance
}

-- Published so panels can render without a call.
funds = 0
reputation = 0

local LEDGER_MAX = 40    -- how many transactions we keep (the panel shows ~8)
local booted = false

-- First run on a slot seeds the opening balance. `co.booted` marks it done, so
-- a company that legitimately spends down to zero is never topped back up.
local function boot()
  if booted then return end
  booted = true
  if not save.get("co.booted") then
    save.set("co.booted", true)
    save.set("co.funds", params.start_funds)
    save.set("co.rep", 0)
    save.set("co.ledger", { { amount = params.start_funds, why = "seed capital", t = 0 } })
  end
  funds = save.get("co.funds") or 0
  reputation = save.get("co.rep") or 0
end

function start(node)
  boot()
end

-- Keep the published mirrors live for anything reading them as plain state.
function update(node, dt)
  boot()
  funds = save.get("co.funds") or 0
  reputation = save.get("co.rep") or 0
end

function balance()
  boot()
  return save.get("co.funds") or 0
end

function rep()
  boot()
  return save.get("co.rep") or 0
end

local function record(amount, why)
  local l = save.get("co.ledger") or {}
  l[#l + 1] = { amount = amount, why = why or "—", t = time }
  while #l > LEDGER_MAX do table.remove(l, 1) end
  save.set("co.ledger", l)
end

-- Can we afford it? Asked by the builder before it lets you launch, so the
-- refusal is a sentence you can act on rather than a balance that went negative.
function afford(amount)
  return balance() >= (amount or 0)
end

-- Spend. Returns false and changes NOTHING if the money isn't there — callers
-- are expected to check, and a half-applied purchase is worse than a refusal.
function spend(amount, why)
  amount = math.max(0, math.floor(amount or 0))
  if amount == 0 then return true end
  local b = balance()
  if b < amount then return false end
  save.set("co.funds", b - amount)
  funds = b - amount
  record(-amount, why)
  log(string.format("company: −$%d  (%s)   balance $%d", amount, why or "—", funds))
  return true
end

function earn(amount, why)
  amount = math.max(0, math.floor(amount or 0))
  if amount == 0 then return end
  local b = balance()
  save.set("co.funds", b + amount)
  funds = b + amount
  record(amount, why)
  log(string.format("company: +$%d  (%s)   balance $%d", amount, why or "—", funds))
end

-- Reputation moves slowly and clamps: it gates the catalogue later (SC6), so
-- it must never run away on a lucky streak or bottom out unrecoverably.
function addRep(d, why)
  local r = math.max(-5, math.min(10, rep() + (d or 0)))
  save.set("co.rep", r)
  reputation = r
  if d and d ~= 0 then
    log(string.format("company: reputation %+d → %d  (%s)", d, r, why or "—"))
  end
end

-- The most recent `n` transactions, newest first, formatted for a panel.
function ledgerLines(n)
  local l = save.get("co.ledger") or {}
  local out = {}
  for i = #l, math.max(1, #l - (n or 8) + 1), -1 do
    local e = l[i]
    out[#out + 1] = string.format("%s$%-7d %s",
      e.amount < 0 and "−" or "+", math.abs(e.amount), e.why or "—")
  end
  return out
end

-- Money, formatted the one way the whole game formats it.
function money(v)
  v = math.floor(v or 0)
  local s = tostring(math.abs(v))
  local out = ""
  while #s > 3 do
    out = "," .. s:sub(-3) .. out
    s = s:sub(1, -4)
  end
  return (v < 0 and "−$" or "$") .. s .. out
end
