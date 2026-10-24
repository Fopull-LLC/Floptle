-- space.belt{...}: a belt of small bodies, made from space.beltOrbits rows.
--
-- The rocks are runtime nodes: made while the game plays, from the seed, and
-- gone when it stops, so a belt is never saved and is the same belt on every
-- machine. Each is a node named `name .. " " .. i` (or an instance of
-- `prefab`), given a CelestialBody on its row's orbit about `parent`.
--
-- With `shape`, each rock is also ground you can land on: a terrain made from
-- the belt's seed when something comes near it, as an irregular small body.
-- `shape` is "potato", "binary", "rubble" or "mixed" (one of the three each,
-- from the seed), a `smallBody` table for terrain.generatePlanet, or a
-- function(i, row) returning one. Rock i is terrain id `terrainId + i - 1`.

local space = space

local KNOWN = {
	parent = true, count = true, seed = true, aMin = true, aMax = true, eMax = true, iMax = true,
	radiusMin = true, radiusMax = true, mu = true, reach = true, name = true, prefab = true, made = true,
	shape = true, terrainId = true, voxel = true,
}

local SHAPES = { "potato", "binary", "rubble" }

-- The smallBody table rock `i` is made with, or nil for no ground.
local function rock_shape(opts, i, row)
	local s = opts.shape
	if s == nil then
		return nil
	end
	if type(s) == "function" then
		s = s(i, row)
		if s == nil then
			return nil
		end
	end
	if type(s) == "string" then
		s = { shape = s }
	end
	if type(s) ~= "table" then
		error("space.belt: shape is a name, a smallBody table or a function returning one", 3)
	end
	local body = {}
	for k, v in pairs(s) do
		body[k] = v
	end
	if body.shape == "mixed" or body.shape == nil then
		body.shape = SHAPES[((opts.seed or 0) * 7919 + i * 104729) % 3 + 1]
	end
	return body
end
local ORBIT = { "count", "seed", "aMin", "aMax", "eMax", "iMax", "radiusMin", "radiusMax" }

function space.belt(opts)
	if type(opts) ~= "table" then
		error("space.belt takes an options table: space.belt{ parent = \"Sun\", count = 200, seed = 1, aMin = 5000, aMax = 8000 }", 2)
	end
	for k in pairs(opts) do
		if not KNOWN[k] then
			error("space.belt: no option '" .. tostring(k) .. "'", 2)
		end
	end
	if type(opts.parent) ~= "string" or opts.parent == "" then
		error("space.belt: parent must name the body the belt orbits", 2)
	end
	local orbit = {}
	for _, k in ipairs(ORBIT) do
		orbit[k] = opts[k]
	end
	local rows = space.beltOrbits(orbit)
	local name, made = opts.name or "Rock", opts.made
	for i, row in ipairs(rows) do
		local function settle(n)
			local c = {
				parent = opts.parent, a = row.a, e = row.e, i = row.i, lan = row.lan,
				argPe = row.argPe, m0 = row.m0, bodyRadius = row.bodyRadius, mu = opts.mu or 0,
			}
			if opts.reach then c.reach = opts.reach end
			n:setCelestial(c)
			local body = rock_shape(opts, i, row)
			if body then
				n:setTerrain((opts.terrainId or 10000) + i - 1)
				n:setTerrainGen({
					seed = (opts.seed or 0) * 100003 + i,
					radius = row.bodyRadius,
					voxel = opts.voxel or math.max(0.25, row.bodyRadius / 24),
					relief = row.bodyRadius * 0.03,
					smallBody = body,
				})
			end
			if made then made(n, i, row) end
		end
		if opts.prefab then
			spawn(opts.prefab, nil, function(n)
				n.name = name .. " " .. i
				settle(n)
			end)
		else
			createNode(name .. " " .. i, settle)
		end
	end
	return #rows
end
