# Build a 2D pixel platformer

A level of tiles with slopes and ledges, and a player who runs and jumps.

**no experience needed** · about 25 minutes · 6 steps

The finished project is a starter template: create a new project with the **platformer2d** template (in the Hub, or `floptle --new <dir> --template platformer2d`) to read the answer.

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

A side-on platformer, made the 2D way: a level painted out of tiles, a player
that is a sprite, and a camera that follows it without ever showing past the
end of the level.

You need two pictures: a **spritesheet of tiles** (16-pixel tiles are a good
start) and a **character**. Draw them in the 🖼 Image tab, or bring in any you
have with **Assets ▸ ⬇ Import…**. To borrow ours, make a project from the
**Pixel Platformer** template and copy `textures/tiles.png` and
`textures/player.png` across.

## 1. A flat stage and a flat camera

Start from a fresh project. Delete the starter crate, ball and capsule.

Select the `Camera`, remove its `freelook` script (the **…** beside it →
**🗑 Remove**), and in the Inspector set **projection** to **orthographic**
with a **height** of about `11`. Set its **rotation** to all zeroes and its
**position** to `8, 6, 20`.

An orthographic camera draws everything at the same size however far away it
is, which is what flat art wants. With 1-unit tiles, the height is how many
tiles tall the view is.

Everything you place from here sits at **z = 0**. Open the scene again and the
Scene view locks itself to the front, the way the game camera looks.

## 2. Paint the level

Open the **◫ Tiles** tab and press **+ Add layer**. Rename the new node
`Level` in the Hierarchy.

Drag your tile spritesheet from Assets onto **TILESET ▸ Drop your spritesheet
here** (or pick it from **choose an image…**). That makes the layer's tileset,
cut into tiles, with each tile's collision worked out from its pixels: solid
blocks are solid, ramps follow their slope, small details are walk-through.

Click a tile in **PALETTE**, press **9** for the tile tool, and paint a floor in
the ⌖ Scene view. Paint past the edge and the layer grows to fit.

### Check the collision

Tick **GRID ▸ Collision** to see what the tiles collide as. To change one,
click it in the palette and pick under **TILE ▸ collides**.

*Done when: a node called Level is in the scene.*

## 3. Gravity

**Add ▸ Δ World ▸ ⬇ Gravity Volume**. On **Down** it pulls everywhere, whatever
its radius. Set its **strength** to about `30`: platformers use far more
gravity than the real 9.81, so that jumps are quick and falls are snappy.

## 4. The player

Drag your character's image from Assets into the Scene view, above the floor.
It becomes a ▫ Sprite where you let go, drawn at the same pixel size as your
tiles. Rename it `Player`.

If the image is a strip of animation frames, set the Sprite's material
**sheetCols** to how many frames there are.

In the Inspector, **➕ Add Component ▸ Physics ▸ ♦ Rigidbody**. On a sprite
that is already the right body: a box the sprite's size, kept in the 2D plane.
Press Play and it falls onto your floor and stays there.

*Done when: you've pressed Play.*

## 5. Run and jump

Create the script below and attach it to `Player`. **A** and **D** run,
**Space** jumps, and a gamepad works too.

### Three things that make a jump feel fair

- **Coyote time.** For a tenth of a second after running off a ledge you can
  still jump. Without it, a jump pressed a frame late does nothing, and the
  player blames the game.
- **Buffering.** A jump pressed just before landing happens on landing.
  `input.buffered("Jump", ticks)` remembers the press and `input.consume`
  uses it up, so it fires once.
- **Cut-off.** Letting go of Jump on the way up keeps only part of the speed,
  so a tap is a hop and a hold is a full jump.

The script also flips the sprite to face the way it runs, and picks a frame:
two for standing, four for running. Change the numbers in the Inspector while
playing to tune the feel.

`scripts/pixelPlayer.lua`

```lua
-- A 2D platformer player. Attach to a Sprite with a 2D box RigidBody
-- (Add RigidBody on a Sprite gives it exactly that).
--
-- The three things that make a jump feel fair:
--   coyote time  a jump still works a moment after running off a ledge
--   buffering    a jump pressed just before landing happens on landing
--   cut-off      letting go of Jump early makes a shorter hop

defaults = {
  --@header Movement
  --@range 0 20 --@units m/s
  speed = 7.0,
  --@range 0 30 --@units m/s
  jump = 14.0,
  --@desc Speed kept when Jump is let go on the way up.
  --@range 0 1
  cutoff = 0.45,
  --@header Forgiveness
  --@range 0 0.3 --@units s
  coyote = 0.1,
  --@desc Ticks a jump press is remembered before landing.
  --@range 0 20
  buffer = 6,
  --@header Look
  --@desc Running frames per second.
  --@range 1 30
  runFps = 12,
}

local sinceGround = 99

function fixedUpdate(node, dt)
  local mx = input.axis2("Move")
  local vy = node.vy

  -- On the ground: the contact flag, or a short probe under the feet.
  local grounded = node.grounded or raycast(node.pos, vec3(0, -1, 0), 0.6) ~= nil
  if grounded then sinceGround = 0 else sinceGround = sinceGround + dt end

  if sinceGround <= params.coyote and input.buffered("Jump", params.buffer) then
    input.consume("Jump")
    vy = params.jump
    sinceGround = 99 -- one jump per ledge
  end
  if vy > 0 and input.justReleased("Jump") then
    vy = vy * params.cutoff
  end

  node.vel = vec3(mx * params.speed, vy, 0)

  -- Face the way you are going, and pick a frame: 0-1 standing, 2-5 running,
  -- 2 in the air.
  local sp = node:sprite()
  if mx ~= 0 then sp.flipX = mx < 0 end
  if not grounded then
    sp.cell = 2
  elseif mx ~= 0 then
    sp.cell = 2 + math.floor(time * params.runFps) % 4
  else
    sp.cell = math.floor(time * 2) % 2
  end

  -- Fell off the world: back to the start.
  if node.pos.y < -10 then
    node.pos = vec3(4, 6, 0)
    node.vel = vec3(0, 0, 0)
  end
end
```

*Done when: Player runs pixelPlayer.lua.*

## 6. Follow the player

Select the `Camera`. Under its projection settings tick **2D camera**, and set
**follow** to `Player`.

Tick **keep inside a rectangle** and set **min** to the level's bottom-left
corner and **max** to its top-right, in world units. The camera keeps its whole
view inside that rectangle, so it never shows past the end of the level.

Press Play and run to the end. You have a platformer.

