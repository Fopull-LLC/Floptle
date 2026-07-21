-- GAME MANAGER — the save-slot player flow (main menu → slot → loading → play).
--
-- The menu scene picks a save slot (save.slot("slotN")) and loads this scene.
-- On start, if a slot is active, this script:
--   1. points terrain persistence at the slot (terrain.saveDir) — digs persist
--      per slot, and previously visited worlds reload from fast slot files,
--   2. reads the slot's galaxy seed (rolling + storing one on first play),
--   3. shows the LOADING overlay and regenerates the whole system from that
--      seed (deterministic — the same slot always gets the same galaxy),
--   4. holds the player safely above the spawn planet until its terrain is
--      actually solid (terrain.query at the surface), then places them —
--      at their saved position if the slot has one, else at the north pole,
--   5. autosaves (position + terrain flush) every few seconds and on the
--      HUD's ☰ MENU button, which returns to the main menu.
--
-- Played DIRECTLY in the editor (no slot selected — save.slot() == "main"),
-- it does nothing: the authored scene behaves exactly as before.

defaults = {
  autosave = 12.0, -- seconds between checkpoint saves
}

-- Published: the menu button + other scripts read/act on these.
loading = false

local gen, overlay, overlay_text
local slot = nil
local seed = 0
local spawn_x, spawn_y, spawn_z, spawn_r = 0, 0, 0, 0
local spawn_body = nil
local next_save = 0
local load_t = 0

local function ui_of(name)
  local n = find(name)
  return n, n and n:getcomponent("UiElement")
end

-- Position saved RELATIVE to the dominant body: absolute coordinates go stale
-- (orbital phases restart with the sim clock), but "standing here on Golil"
-- doesn't. Restores place the node at the body's LIVE position + the offset.
local function save_pos_of(node, prefix)
  local d = space.dominant(node.x, node.y, node.z)
  local b = d and space.body(d)
  if not b then return end
  save.set(prefix .. "_body", d)
  save.set(prefix .. "_x", node.x - b.x)
  save.set(prefix .. "_y", node.y - b.y)
  save.set(prefix .. "_z", node.z - b.z)
end

local function restore_pos_of(node, prefix)
  local bodyname = save.get(prefix .. "_body")
  if not bodyname then return false end
  local b = space.body(bodyname)
  if not b then return false end
  node.x = b.x + (save.get(prefix .. "_x") or 0)
  node.y = b.y + (save.get(prefix .. "_y") or 0)
  node.z = b.z + (save.get(prefix .. "_z") or 0)
  node.vx, node.vy, node.vz = 0, 0, 0
  return true
end

-- The full checkpoint: player + ship positions and every dug field → the slot.
function saveGame()
  if not slot then return end
  local astro = find("Astronaut")
  if astro then save_pos_of(astro, "p") end
  -- The scout is a stashed debug tool: only track it while it's summoned.
  local sc = findScript("ship_controller")
  local ship = find("Ship")
  if ship and not (sc and sc.stashed) then save_pos_of(ship, "s") end
  save.set("g_played", (save.get("g_played") or 0) + 1)
  terrain.flush()
end

-- The ☰ MENU button calls this through a script handle.
function exitToMenu()
  saveGame()
  scene.load("menu")
end

function start(node)
  local s = save.slot()
  if s == "main" then return end -- direct editor play: stay out of the way
  slot = s
  terrain.saveDir("saves/" .. slot .. "/terrain")
  gen = findScript("system_generator")
  if not gen then
    print("game_manager: no System Generator in the scene — slot flow disabled")
    slot = nil
    return
  end
  seed = save.get("g_seed") or 0
  loading = true
  load_t = time
  -- Nothing simulates until the world is actually there: physics is paused
  -- for the whole loading screen (scripts, rails and terrain streaming keep
  -- running), so nothing can fall through half-streamed terrain or get
  -- ejected before the ground exists. Resumed when the crew is placed.
  physics.pause(true)
  for _, nm in ipairs({ "Loading Screen", "Loading Text" }) do
    local _, el = ui_of(nm)
    if el then el.visible = true end
  end
  -- Regenerate THIS slot's galaxy. Deterministic per seed + the parameters the
  -- player picked when they created the save (nil = the generator's default).
  -- A save that has checkpointed before (g_played) skips the spawn planet's
  -- eager regeneration: its terrain streams back from the slot's saved field,
  -- so digs survive; a brand-new save generates it first — and alone.
  gen.regenerate(seed, {
    pregen = save.get("g_played") == nil,
    planets = save.get("g_planets"),
    moonChance = save.get("g_moonchance"),
    atmoChance = save.get("g_atmo"),
    caveScale = save.get("g_caves"),
    minOrbit = save.get("g_minorbit"),
    spacing = save.get("g_spacing"),
    radiusMin = save.get("g_radmin"),
    radiusMax = save.get("g_radmax"),
    gravityMin = save.get("g_gravmin"),
    gravityMax = save.get("g_gravmax"),
    starScale = save.get("g_star"),
  })
  if seed <= 0 then
    seed = gen.lastSeed
    save.set("g_seed", seed)
  end
  spawn_body = gen.lastSpawnBody
  spawn_x, spawn_y, spawn_z = gen.lastSpawnX, gen.lastSpawnY, gen.lastSpawnZ
  spawn_r = gen.lastSpawnRadius
end

-- The spawn planet ORBITS while the loading screen runs (rails advance with
-- sim time), so its roll-time coordinates go stale within seconds — holding
-- or dropping the crew at those stranded them in empty space while the planet
-- sailed away. Every spawn-relative position must come from the body's LIVE
-- rails position; the roll numbers are only the fallback for the first frames
-- before the generated body registers with the rails system.
local function spawn_live()
  local b = spawn_body and space.body(spawn_body)
  if b then return b.x, b.y, b.z end
  return spawn_x, spawn_y, spawn_z
end

function update(node, dt)
  if not slot then return end

  if loading then
    -- Hold the crew safely above the spawn planet (its LIVE position — it is
    -- orbiting under us right now) while its terrain streams in. Velocity
    -- pinned to zero: the world can't hurt what physics can't reach.
    local sx, sy, sz = spawn_live()
    local hover = sy + spawn_r + 60
    local sc = findScript("ship_controller")
    for _, nm in ipairs({ "Astronaut", "Ship" }) do
      -- A stashed scout stays parked in deep space — don't drag it along.
      if nm ~= "Ship" or not (sc and sc.stashed) then
        local n = find(nm)
        if n then
          n.x, n.y, n.z = sx + (nm == "Ship" and 14 or 0), hover, sz
          n.vx, n.vy, n.vz = 0, 0, 0
        end
      end
    end
    local ttl = find("Loading Text")
    if ttl then
      local dots = string.rep("·", 1 + math.floor((time - load_t) * 2) % 4)
      ttl.text = string.format("ENTERING GALAXY %d %s", seed, dots)
    end
    -- Ready = the spawn planet's SURFACE answers: the signed distance at its
    -- (live) north-pole surface point is small only once ITS field has
    -- collision.
    local d = terrain.query(sx, sy + spawn_r, sz)
    if d and math.abs(d) < spawn_r * 0.5 and time - load_t > 1.0 then
      loading = false
      physics.pause(false)
      local astro = find("Astronaut")
      local vp = nil
      for _, s in ipairs(findScripts("vessel_controller")) do
        if s.piloting then vp = s break end
      end
      if (save.get("shipyard.pilot") or 0) == 1 or vp then
        -- Arriving from the builder's LAUNCH: the astronaut starts IN the
        -- vessel's pod (the vessel pilot seats them the moment it spawns) —
        -- never standing on the surface next to their own rocket.
        if astro then
          astro.visible = false
          astro.x, astro.y, astro.z = sx, sy + spawn_r + 6, sz
          astro.vx, astro.vy, astro.vz = 0, 0, 0
        end
      elseif astro and not restore_pos_of(astro, "p") then
        astro.x, astro.y, astro.z = sx, sy + spawn_r + 6, sz
        astro.vx, astro.vy, astro.vz = 0, 0, 0
      end
      local ship = find("Ship")
      local sc2 = findScript("ship_controller")
      if sc2 and sc2.stashed then
        ship = nil -- summon it with L when you want it
      end
      if ship and not restore_pos_of(ship, "s") then
        ship.x, ship.y, ship.z = sx + 14, sy + spawn_r + 4, sz
        ship.vx, ship.vy, ship.vz = 0, 0, 0
      end
      for _, nm in ipairs({ "Loading Screen", "Loading Text" }) do
        local _, lel = ui_of(nm)
        if lel then lel.visible = false end
      end
      next_save = time + params.autosave
      print(string.format("galaxy %d ready — welcome to %s", seed, spawn_body or "?"))
    end
    return
  end

  -- Autosave checkpoints.
  if time >= next_save then
    next_save = time + params.autosave
    saveGame()
  end
end
