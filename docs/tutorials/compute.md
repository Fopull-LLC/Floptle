# Compute shaders — a pond the GPU simulates

Ripples spreading across a pond, simulated on the GPU, 65,000 points every step.

**some coding** · about 30 minutes · 8 steps

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

A **compute shader** is a program that runs on the GPU and draws nothing: it
reads and writes buffers and textures, every pixel or particle at once. Here
it simulates a pond. Raindrops land, ripples spread and fade, and the water is
lit by its own slope, 65,536 points every step. The same thing in a Lua loop
would take most of a frame; on the GPU it is a fraction of a millisecond.

## The idea

**You run it; it doesn't run itself.** Your script calls `compute.run` with a
shader file, an entry point, how many invocations, and what to bind. The
engine runs it before the frame draws, so anything drawn that frame sees the
result.

**The engine writes the boring part.** Every buffer and texture you bind is
there in the shader under the name you bound it to, with `params` beside them.
You never count binding numbers.

## What you need first

Some Lua, and the [compute reference](../scripting/compute.md) open in a tab
for later. Shaders are WGSL, the language WebGPU uses; you'll see all of it
you need here.

## 1. A pond to draw on

Add a **Plane** and name it `Pond`. Point the camera down at it, so it fills
most of the view. That is the whole scene; everything else happens on the GPU.

*Done when: a node called Pond is in the scene.*

## 2. Your first compute shader

Shaders live in `shaders/`, as `.wgsl` files. This one has one entry point,
`paint`, run once for every pixel of a picture.

- `@workgroup_size(8, 8)` is how many invocations run together: 64, as an 8×8
  tile. 64 is a good size on every GPU.
- `id` is which invocation this is: here, which pixel.
- `picture` and `params` are not declared anywhere in the file. The engine
  declares them, from what the script binds.
- The first line of the body bails out past the picture's edge. The engine
  rounds the number of tiles up, so the last ones hang off the side.

`shaders/pond.wgsl`

```wgsl
// shaders/pond.wgsl: one entry point, run for every pixel of the picture.
@compute @workgroup_size(8, 8)
fn paint(@builtin(global_invocation_id) id: vec3<u32>) {
  let size = textureDimensions(picture);
  if (id.x >= size.x || id.y >= size.y) { return; }
  let uv = vec2<f32>(id.xy) / vec2<f32>(size);
  let wave = 0.5 + 0.5 * sin(uv.x * 12.0 + params.time * params.speed);
  let water = mix(vec3<f32>(0.02, 0.12, 0.25), vec3<f32>(0.3, 0.7, 1.0), wave * uv.y);
  textureStore(picture, vec2<i32>(id.xy), vec4<f32>(water, 1.0));
}
```

## 3. Run it from a script

Attach a new script, `pond`, to `Pond`, and press Play. The pond fills with
moving bands of blue.

- `gpu.texture` makes a picture on the GPU. Its `name` (`gpu:1`) works
  anywhere a texture path does, so `setMaterial` puts it on the plane.
- `compute.run` runs `paint` over `size` invocations, one per pixel, with
  `picture` bound to the texture and `params.speed` set to 2.
- `params.time` is always there: seconds of play.

Change `speed`, save, and watch it change. Change the shader and save it: it
reloads while you play.

`scripts/pond.lua`

```lua
-- The pond: a picture the GPU paints, every frame.
local SIZE = 256
local picture

function start(node)
  picture = gpu.texture{ w = SIZE, h = SIZE }
  node:setMaterial{ texture = picture.name }
end

function update(node, dt)
  compute.run("shaders/pond.wgsl", "paint", {
    size = { SIZE, SIZE },
    bind = { picture = picture },
    params = { speed = 2 },
  })
end
```

*Done when: Pond runs pond.lua.*

## 4. Ripples need memory

A ripple is a height that spreads to its neighbours, step by step, so the
shader needs last step's heights to make this step's. That memory goes in
**buffers**: arrays on the GPU, here one `vec2` per point of the pond, this
step's height and the last one.

A shader can't safely read a buffer while it writes the same one: one
invocation would read a neighbour another had already changed. So there are
two, `now` and `later`. `ripple` reads `now` and writes `later`, and the
script swaps them after each step. This is called **ping-pong**.

`paint` now lights the water by its slope, read from `now`. Copy this over
your `pond.wgsl`.

`cell` is an ordinary function. It can use `params` because each run gives a
helper the same bindings as the entry point that calls it.

`shaders/pond.wgsl`

```wgsl
// shaders/pond.wgsl: ripples on a pond.
//
// `now` holds each point's height this step (x) and the step before (y).
// `ripple` reads `now` and writes `later`; the script swaps the two.

fn cell(x: i32, y: i32) -> u32 {
  let n = i32(params.size);
  return u32(clamp(y, 0, n - 1) * n + clamp(x, 0, n - 1));
}

@compute @workgroup_size(8, 8)
fn ripple(@builtin(global_invocation_id) id: vec3<u32>) {
  let n = u32(params.size);
  if (id.x >= n || id.y >= n) { return; }
  let x = i32(id.x);
  let y = i32(id.y);
  let here = now[cell(x, y)];
  let around = now[cell(x - 1, y)].x + now[cell(x + 1, y)].x + now[cell(x, y - 1)].x + now[cell(x, y + 1)].x;
  // The classic ripple: half the neighbours, less where it was a step ago.
  var h = (around * 0.5 - here.y) * params.damping;
  // A raindrop, when the script drops one.
  let d = vec2<f32>(f32(x), f32(y)) - vec2<f32>(params.dropX, params.dropY);
  if (params.drop > 0.5 && dot(d, d) < 9.0) {
    h = 1.0;
  }
  later[cell(x, y)] = vec2<f32>(h, here.x);
}

@compute @workgroup_size(8, 8)
fn paint(@builtin(global_invocation_id) id: vec3<u32>) {
  let n = u32(params.size);
  if (id.x >= n || id.y >= n) { return; }
  let x = i32(id.x);
  let y = i32(id.y);
  // Lit by its slope, so a ripple reads as a ripple.
  let slope = vec2<f32>(now[cell(x + 1, y)].x - now[cell(x - 1, y)].x, now[cell(x, y + 1)].x - now[cell(x, y - 1)].x);
  let light = clamp(0.5 + dot(slope, vec2<f32>(0.6, 0.8)) * 3.0, 0.0, 1.0);
  let water = mix(vec3<f32>(0.02, 0.12, 0.25), vec3<f32>(0.55, 0.85, 1.0), light);
  textureStore(picture, vec2<i32>(id.xy), vec4<f32>(water, 1.0));
}
```

## 5. Swap the buffers, drop the rain

Copy this over `pond.lua` and press Play: raindrops land, and their rings
spread and fade.

- Two `gpu.buffer`s of `SIZE × SIZE` `vec2`s, zeroed: a still pond.
- The ripple runs in `fixedUpdate`, so it steps at the same rate on every
  machine and the ripples spread at the same speed however fast the frames
  come. Painting is `update`'s job, once a frame.
- A drop is three numbers in `params`: whether to drop, and where. The script
  picks them; the shader does the splash.
- `now, later = later, now` is the ping-pong. Calls run in the order you make
  them, each after the last finished, so `paint` sees the step that just ran.

Try `damping = 0.995` for a glassier pond, or `0.95` for treacle.

`scripts/pond.lua`

```lua
-- The pond: ripples simulated on the GPU, raindrops dropped from here.
local SIZE = 256
local now, later, picture
local untilDrop = 0

function start(node)
  -- Two buffers of heights: one is read while the other is written.
  now = gpu.buffer{ count = SIZE * SIZE, type = "vec2" }
  later = gpu.buffer{ count = SIZE * SIZE, type = "vec2" }
  picture = gpu.texture{ w = SIZE, h = SIZE }
  node:setMaterial{ texture = picture.name }
end

-- The simulation steps at a fixed rate, so ripples spread at the same
-- speed however fast the frames come.
function fixedUpdate(node, dt)
  local drop, x, y = 0, 0, 0
  untilDrop = untilDrop - dt
  if untilDrop <= 0 then
    untilDrop = 0.2 + math.random() * 0.6
    drop, x, y = 1, math.random(8, SIZE - 8), math.random(8, SIZE - 8)
  end
  compute.run("shaders/pond.wgsl", "ripple", {
    size = { SIZE, SIZE },
    bind = { now = now, later = later },
    params = { size = SIZE, damping = 0.985, drop = drop, dropX = x, dropY = y },
  })
  now, later = later, now
end

function update(node, dt)
  compute.run("shaders/pond.wgsl", "paint", {
    size = { SIZE, SIZE },
    bind = { now = now, picture = picture },
    params = { size = SIZE },
  })
end
```

*Done when: pond.lua swaps its two buffers.*

## 6. Count on the GPU

How much of the pond is moving? Every pixel knows its own answer, and 65,536
of them want to add to one total at the same moment. An ordinary write would
lose most of them. An **atomic** add can't: each one lands.

`paint` now adds one to `moving[0]` for every pixel whose height is not close
to still. Copy this over `pond.wgsl`; the script in the next step binds
`moving`.

`shaders/pond.wgsl`

```wgsl
// shaders/pond.wgsl: ripples on a pond, and how much of it is moving.
//
// `now` holds each point's height this step (x) and the step before (y).
// `ripple` reads `now` and writes `later`; the script swaps the two.

fn cell(x: i32, y: i32) -> u32 {
  let n = i32(params.size);
  return u32(clamp(y, 0, n - 1) * n + clamp(x, 0, n - 1));
}

@compute @workgroup_size(8, 8)
fn ripple(@builtin(global_invocation_id) id: vec3<u32>) {
  let n = u32(params.size);
  if (id.x >= n || id.y >= n) { return; }
  let x = i32(id.x);
  let y = i32(id.y);
  let here = now[cell(x, y)];
  let around = now[cell(x - 1, y)].x + now[cell(x + 1, y)].x + now[cell(x, y - 1)].x + now[cell(x, y + 1)].x;
  var h = (around * 0.5 - here.y) * params.damping;
  let d = vec2<f32>(f32(x), f32(y)) - vec2<f32>(params.dropX, params.dropY);
  if (params.drop > 0.5 && dot(d, d) < 9.0) {
    h = 1.0;
  }
  later[cell(x, y)] = vec2<f32>(h, here.x);
}

@compute @workgroup_size(8, 8)
fn paint(@builtin(global_invocation_id) id: vec3<u32>) {
  let n = u32(params.size);
  if (id.x >= n || id.y >= n) { return; }
  let x = i32(id.x);
  let y = i32(id.y);
  let slope = vec2<f32>(now[cell(x + 1, y)].x - now[cell(x - 1, y)].x, now[cell(x, y + 1)].x - now[cell(x, y - 1)].x);
  let light = clamp(0.5 + dot(slope, vec2<f32>(0.6, 0.8)) * 3.0, 0.0, 1.0);
  let water = mix(vec3<f32>(0.02, 0.12, 0.25), vec3<f32>(0.55, 0.85, 1.0), light);
  textureStore(picture, vec2<i32>(id.xy), vec4<f32>(water, 1.0));
  // Every pixel that is moving adds one, all of them at once.
  if (abs(now[cell(x, y)].x) > 0.02) {
    atomicAdd(&moving[0], 1u);
  }
}
```

## 7. Read the answer back

Copy this over `pond.lua` and press Play. Every two seconds the Console says
how much of the pond is moving.

- `type = "atomic"` makes a buffer of numbers shaders add to with
  `atomicAdd`. From Lua it reads and writes like any whole number.
- `moving:write({ 0 })` zeroes it before each `paint`. Writes and runs keep
  the order you made them in.
- `moving:read(fn)` hands the numbers back a frame or two later. The frame
  never waits for the GPU: that is the price of reading back, and why it is a
  callback rather than an answer.

`scripts/pond.lua`

```lua
-- The pond: ripples simulated on the GPU, and how busy it is.
local SIZE = 256
local now, later, picture, moving
local untilDrop = 0
local untilCount = 0

function start(node)
  now = gpu.buffer{ count = SIZE * SIZE, type = "vec2" }
  later = gpu.buffer{ count = SIZE * SIZE, type = "vec2" }
  picture = gpu.texture{ w = SIZE, h = SIZE }
  -- One number every invocation can add to at once.
  moving = gpu.buffer{ count = 1, type = "atomic" }
  node:setMaterial{ texture = picture.name }
end

function fixedUpdate(node, dt)
  local drop, x, y = 0, 0, 0
  untilDrop = untilDrop - dt
  if untilDrop <= 0 then
    untilDrop = 0.2 + math.random() * 0.6
    drop, x, y = 1, math.random(8, SIZE - 8), math.random(8, SIZE - 8)
  end
  compute.run("shaders/pond.wgsl", "ripple", {
    size = { SIZE, SIZE },
    bind = { now = now, later = later },
    params = { size = SIZE, damping = 0.985, drop = drop, dropX = x, dropY = y },
  })
  now, later = later, now
end

function update(node, dt)
  moving:write({ 0 })
  compute.run("shaders/pond.wgsl", "paint", {
    size = { SIZE, SIZE },
    bind = { now = now, picture = picture, moving = moving },
    params = { size = SIZE },
  })
  untilCount = untilCount - dt
  if untilCount <= 0 then
    untilCount = 2
    -- Answered a frame or two later, without the frame waiting for it.
    moving:read(function(values)
      log(string.format("%d%% of the pond is moving", values[1] * 100 // (SIZE * SIZE)))
    end)
  end
end
```

*Done when: pond.lua makes an atomic buffer.*

## 8. What it costs, and where it runs

Press the **⏱** button in the play toolbar while it plays: among the GPU's
passes, each run appears as `compute ripple` and `compute paint`, well under
a millisecond each at this size. A script can ask the same with
`perf.gpuMs("compute paint")`.

- **Size.** At 256×256 the GPU barely notices. A 1024×1024 pond is 16 times
  the work and still small. Readbacks are the thing to ration: one a second
  is plenty for a number a player reads.
- **Everywhere.** Compute runs on Windows, macOS, Linux and in the browser.
  The browser's limits are the tightest (`gpu.limits()` says what a device
  allows), and a game that keeps within them runs everywhere.
- **Not for gameplay rules.** GPUs differ slightly in their arithmetic and a
  dedicated server has none, so use compute for what players see, not for
  state a multiplayer match must agree on.

From here: particles a hundred thousand strong, falling sand, a heat map
painted onto terrain. The [compute reference](../scripting/compute.md) has a
particle swarm and the game of life to start from.

