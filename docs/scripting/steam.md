# Steam

Leaderboards, lobbies, and the overlay.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [28a. Steam leaderboards: `steam.*`](#28a-steam-leaderboards-steam)
- [28b. Steam lobbies: finding other players](#28b-steam-lobbies-finding-other-players)
- [28c. The Steam overlay](#28c-the-steam-overlay)

---

## 28a. Steam leaderboards: `steam.*`

Not to be confused with §27's — `account.*` leaderboards live on **fopull.com**
and work for every player of your game; these are **Steam's**, and exist only
for players who launched through Steam. A game can use either, or both.

```lua
steam.findLeaderboard("HIGH_SCORES", function(board, err)
  if err then log(err) return end
  if not board then log("no such board") return end   -- nil, no error

  steam.uploadScore(board.id, score, function(res)
    if res and res.changed then log("new personal best: " .. res.score) end
  end)

  steam.downloadScores(board.id, { scope = "friends", count = 5 }, function(rows)
    for _, r in ipairs(rows or {}) do print(r.rank, r.userId, r.score) end
  end)
end)
```

**Every one of these answers through a callback, on a later frame, exactly
once.** Including when the player isn't on Steam at all — there the callback
gets `(nil, "Steam isn't available in this session")`. That's the whole reason
it works this way: the obvious alternative is to fail immediately when there's
no Steam and call back when there is, which leaves your game with two failure
paths, and the second one is the one nobody remembers to write.

Three things worth knowing before you build a scoreboard on it:

- **`board.id` is a string, and it only lasts this session.** Steam can read a
  handle's raw value but can't turn one back into a handle, so there's nothing
  useful to save. Call `steam.findLeaderboard` again next run.
- **`nil` with no error means "no board by that name."** That's a successful
  answer to a real question, not a failure — branch on it, don't report it.
- **`uploadScore` defaults to `keepBest`**, so `res.score` is what is now
  *stored*, which may not be what you just uploaded. `res.changed` is how a
  "new best!" banner knows.

`scope = "aroundUser"` counts ranks **relative to the player's own** — `start =
-4` with `count = 9` gives them plus the four either side. `steam.leaderboardsInFlight()`
is what a "loading scores…" spinner hangs off.

Creating boards from `steam.findOrCreateLeaderboard` is a development
convenience. A shipping game's boards are normally declared on the Steamworks
admin site, where you can also reset and moderate them.

---

## 28b. Steam lobbies: finding other players

A lobby is **discovery, not transport**. It is how players find each other and
agree on what they are about to play — a small key/value table plus a member
list. It decides nothing about how your game's packets travel; meet in a lobby,
then run the session over whatever `net.*` transport you like (§16).

```lua
-- Host
steam.createLobby({ kind = "public", maxMembers = 8 }, function(lobby, err)
  if not lobby then log(err) return end
  steam.setLobbyData(lobby.id, "mode", "coop")     -- what searches match on
  steam.setLobbyData(lobby.id, "map", "dust")
end)

-- Everyone else
steam.findLobbies({ match = { mode = "coop" }, openSlots = 1 }, function(found)
  for _, l in ipairs(found or {}) do
    print(l.data.map, l.memberCount .. "/" .. l.memberLimit)
  end
end)

steam.joinLobby(id, function(lobby, err)
  if not lobby then log(err) end          -- "that lobby is full", etc.
end)
```

Same rules as §28a: every one of these calls back **on a later frame, exactly
once**, including when the player isn't on Steam. Ids are strings.

**Two kinds of data, with different permissions.** `setLobbyData` is the
lobby's own — the mode, the map, what a search matches — and **only the owner
may change it**. `setLobbyMemberData` is each player's own, and anyone may set
theirs; that's where a ready flag or a chosen character goes.

```lua
steam.onLobbyEvent(function(e)
  if e.kind == "member" then
    print(e.user, e.change)               -- entered / left / disconnected / kicked / banned
  elseif e.whose == "member" then
    print(steam.lobbyMemberData(e.lobby, e.member, "ready"))
  else
    print(steam.lobbyData(e.lobby).mode)  -- the lobby's own data changed
  end
end)
```

`steam.setLobbyJoinable(id, false)` closes the lobby when the match starts.
`steam.lobbyOwner`, `steam.lobbyMembers` and `steam.lobbyMemberLimit` fill in
the rest of a lobby screen, and `steam.lobbyData(id)` with no key hands you the
whole table at once.

Two things Steam itself won't tell you, so this doesn't pretend to: **who
performed a kick** (Steam's binding fills that field with the kicked player's
own id, so reporting it would be wrong exactly when it mattered), and **lobby
chat** — Steam's message handle is only valid inside its own callback, which is
not where this engine can read it. Lobby *data* is the channel to use.

---

## 28c. The Steam overlay

The overlay is the Shift+Tab UI the Steam client draws *over* your game, from
outside your process. Your script never renders it; it only asks for a page, and
hears when the player opens or closes it.

```lua
-- A "Community" button on the pause menu:
local ok, why = steam.openOverlay("community")
if not ok then
  find("Hint").text = why       -- show the URL instead, don't strand them
end

-- The invite button on a lobby screen:
steam.openInviteDialog(lobby.id)

-- Your own store page, or a DLC's by id:
steam.openOverlayStore()
steam.openOverlayStore(1234560)

-- Pause a single-player game while they shop:
steam.onOverlayChanged(function(active)
  paused = active
end)
```

The pages are the SDK's own names: `"friends"`, `"community"`, `"players"`,
`"settings"`, `"officialgamegroup"`, `"stats"`, `"achievements"`.
`steam.openOverlayUser(dialog, id)` opens a page about one user — `"steamid"` is
their profile — and `steam.openOverlayUrl(url)` opens the overlay's browser at a
full `http(s)://` URL.

Three things worth knowing:

- **A misspelt page is refused in every session**, with the valid names in the
  message — you do not need Steam running to find the typo. The backend is never
  asked.
- **`(false, why)` is what "it couldn't open" looks like.** Steam's own call
  silently does nothing when the overlay is disabled in the player's settings,
  hasn't hooked your renderer yet at startup, or can't inject on their setup
  (some Linux/Proton configurations). The engine reports it instead, so a
  purchase or invite button degrades — show the URL, show the lobby code —
  rather than doing nothing. `steam.overlayEnabled()` is the same answer as a
  query.
- **While the overlay is up, your scripts get neutral input**, the same way they
  do when the Game view isn't focused. A key held through Shift+Tab is released,
  not stuck down. The simulation keeps running — a networked session cannot
  pause for one player — so pausing a single-player game is yours to do, from
  `steam.onOverlayChanged` or `steam.overlayActive()`.
