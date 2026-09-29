-- cloud.rank / cloud.docs / cloud.blobs / cloud.counter: the Cloud data
-- primitives by name rather than by path. Each is the documented path with the
-- URL encoding, the envelope and the error sentence done once: reads go out
-- with the game key (cloud.get), or as the player for a private collection,
-- and writes always go out as the signed-in player (account.*).
--
-- Every callback is `function(result, err)`: `err` is nil on success, and
-- otherwise `{ code = "key_taken", message = "...", status = 409 }` for every
-- call alike.

local cloud = cloud

local function enc(s)
	return (string.gsub(tostring(s), "[^A-Za-z0-9%-_%.~]", function(c)
		return string.format("%%%02X", string.byte(c))
	end))
end

local function query(t)
	local keys = {}
	for k, v in pairs(t) do
		if v ~= nil then
			keys[#keys + 1] = k
		end
	end
	if #keys == 0 then
		return ""
	end
	table.sort(keys)
	local parts = {}
	for _, k in ipairs(keys) do
		parts[#parts + 1] = enc(k) .. "=" .. enc(t[k])
	end
	return "?" .. table.concat(parts, "&")
end

local function slug()
	local g = cloud.game()
	if not g then
		error("this project is not connected to Floptle Cloud, so it has no game to name. Connect it in ⚙ Settings ▸ Networked", 3)
	end
	return enc(g)
end

local function names(set)
	local out = {}
	for k in pairs(set) do
		out[#out + 1] = k
	end
	table.sort(out)
	return table.concat(out, ", ")
end

-- `(opts, cb)`, `(cb)` or `()`, with every option key checked.
local function args(opts, cb, accepted, call)
	if type(opts) == "function" then
		return {}, opts
	end
	if opts ~= nil and type(opts) ~= "table" then
		error(call .. ": the options are a table, like { limit = 25 }", 3)
	end
	opts = opts or {}
	for k in pairs(opts) do
		if not accepted[k] then
			error(call .. ": no option '" .. tostring(k) .. "' (accepted: " .. names(accepted) .. ")", 3)
		end
	end
	return opts, cb
end

local function reply(cb, pick)
	return function(res)
		if not cb then
			return
		end
		if res.ok then
			if pick then
				cb(pick(res), nil)
			else
				cb(res.json, nil)
			end
			return
		end
		local j = res.json
		local code = type(j) == "table" and j.error or nil
		if type(code) ~= "string" then
			code = (res.status and res.status > 0) and ("http_" .. res.status) or "network"
		end
		local message = type(j) == "table" and (j.message or j.error_description) or nil
		if type(message) ~= "string" then
			message = res.error or code
		end
		cb(nil, { code = code, message = message, status = res.status })
	end
end

local function name_arg(v, call, example)
	if type(v) ~= "string" or v == "" then
		error(call .. " takes a name, like " .. example, 3)
	end
	return v
end

local function key_arg(v, call)
	if type(v) ~= "string" and type(v) ~= "number" then
		error(call .. ": the key is a string", 3)
	end
	return enc(v)
end

local function me(call)
	local p = account.player()
	if not p then
		error(call .. " needs a signed-in player (account.signIn())", 3)
	end
	return p.id
end

-- ── rankings ────────────────────────────────────────────────────────────────

local Rank = {}
Rank.__index = Rank

function cloud.rank(board)
	return setmetatable({ board = name_arg(board, "cloud.rank", "\"laps\" or \"laps:canyon\"") }, Rank)
end

function Rank:path(tail)
	return "/games/" .. slug() .. "/rank/" .. enc(self.board) .. (tail or "")
end

local PAGE = { limit = true, offset = true, around = true, player = true, sort = true }

function Rank:page(opts, cb)
	opts, cb = args(opts, cb, PAGE, "rank:page")
	cloud.get(self:path(query(opts)), reply(cb))
end

local SUBMIT = { meta = true, blob = true }

function Rank:submit(value, opts, cb)
	if type(value) ~= "number" or value ~= value or value == math.huge or value == -math.huge then
		error("rank:submit: the value is a finite number", 2)
	end
	opts, cb = args(opts, cb, SUBMIT, "rank:submit")
	account.post(self:path(), { value = value, meta = opts.meta, blob = opts.blob }, reply(cb))
end

function Rank:remove(player, cb)
	if type(player) == "function" then
		player, cb = nil, player
	end
	local sub = player or me("rank:remove")
	account.delete(self:path("/" .. enc(sub)), reply(cb))
end

-- ── docs and blobs ──────────────────────────────────────────────────────────

local LIST = { prefix = true, owner = true, sort = true, after = true, limit = true }
local WRITE = { ifVersion = true }
local COLLECTION = { private = true }

local function collection(kind, name, opts, call)
	name = name_arg(name, call, "\"stages\"")
	opts = args(opts, nil, COLLECTION, call)
	return { name = name, kind = kind, private = opts.private == true }
end

local function read(self, path, cb)
	if self.private then
		account.get(path, cb)
	else
		cloud.get(path, cb)
	end
end

local Docs = {}
Docs.__index = Docs

function cloud.docs(name, opts)
	return setmetatable(collection("data", name, opts, "cloud.docs"), Docs)
end

local Blobs = {}
Blobs.__index = Blobs

function cloud.blobs(name, opts)
	return setmetatable(collection("blobs", name, opts, "cloud.blobs"), Blobs)
end

local function base(self)
	return "/games/" .. slug() .. "/" .. self.kind .. "/" .. enc(self.name)
end

local function list(self, opts, cb, call)
	opts, cb = args(opts, cb, LIST, call)
	read(self, base(self) .. query(opts), reply(cb))
end

function Docs:list(opts, cb)
	list(self, opts, cb, "docs:list")
end

function Docs:get(key, cb)
	read(self, base(self) .. "/" .. key_arg(key, "docs:get"), reply(cb))
end

function Docs:put(key, data, opts, cb)
	local k = key_arg(key, "docs:put")
	opts, cb = args(opts, cb, WRITE, "docs:put")
	account.put(base(self) .. "/" .. k, { data = data, if_version = opts.ifVersion }, reply(cb))
end

function Docs:delete(key, cb)
	account.delete(base(self) .. "/" .. key_arg(key, "docs:delete"), reply(cb))
end

function Blobs:list(opts, cb)
	list(self, opts, cb, "blobs:list")
end

function Blobs:get(key, cb)
	read(self, base(self) .. "/" .. key_arg(key, "blobs:get"), reply(cb, function(res)
		return res.body
	end))
end

function Blobs:put(key, bytes, opts, cb)
	local k = key_arg(key, "blobs:put")
	if type(bytes) ~= "string" then
		error("blobs:put: a blob is bytes: pass a string (a file's contents, a picture's res.body)", 2)
	end
	opts, cb = args(opts, cb, WRITE, "blobs:put")
	account.put(base(self) .. "/" .. k .. query({ if_version = opts.ifVersion }), bytes, reply(cb))
end

function Blobs:delete(key, cb)
	account.delete(base(self) .. "/" .. key_arg(key, "blobs:delete"), reply(cb))
end

-- ── counters ────────────────────────────────────────────────────────────────

local Counter = {}
Counter.__index = Counter

function cloud.counter(name)
	return setmetatable({ name = name_arg(name, "cloud.counter", "\"installs\"") }, Counter)
end

function Counter:path(tail)
	return "/games/" .. slug() .. "/count/" .. enc(self.name) .. (tail or "")
end

function Counter:add(key, n, cb)
	if type(n) == "function" then
		n, cb = nil, n
	end
	n = n or 1
	if type(n) ~= "number" or n ~= math.floor(n) then
		error("counter:add: the amount is a whole number", 2)
	end
	account.post(self:path("/" .. key_arg(key, "counter:add")), { add = n }, reply(cb))
end

function Counter:get(key, cb)
	cloud.get(self:path("/" .. key_arg(key, "counter:get")), reply(cb))
end

local TOP = { prefix = true, sort = true, limit = true }

function Counter:top(opts, cb)
	opts, cb = args(opts, cb, TOP, "counter:top")
	cloud.get(self:path(query(opts)), reply(cb))
end
