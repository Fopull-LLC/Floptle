# Compute shaders

[← Scripting guide](../scripting.md)

## 31. Compute shaders: `gpu.*` & `compute.run`

A **compute shader** is a program that runs on the GPU and draws nothing. It
reads and writes buffers and textures, thousands of times at once: a
procedural texture, a particle swarm, falling sand, a simulation, a skin
painted onto a model. What takes a Lua loop a second takes a compute shader a
fraction of a millisecond.

Three pieces:

- **`gpu.buffer`** and **`gpu.texture`** make the things a shader works on.
- **`compute.run`** runs a shader over them, before the frame is drawn, so
  anything drawn that frame sees the result.
- **`:read`** hands the results back to your script a frame or two later. It
  never stalls the frame.

```lua
local cells = gpu.buffer{ count = 256, type = "u32" }
cells:write({ 5, 6, 7 })
compute.run("shaders/double.wgsl", "double", { size = { 256 }, bind = { cells = cells } })
cells:read(function(values) log(values[1]) end)   -- 10, a frame or two later
```

```wgsl
// shaders/double.wgsl
@compute @workgroup_size(64)
fn double(@builtin(global_invocation_id) id: vec3<u32>) {
  cells[id.x] = cells[id.x] * 2u;
}
```

### Buffers

`gpu.buffer{ count = n, type = "f32" }` is a typed array of `n` elements on
the GPU, zeroed. `type` is `f32`, `u32`, `i32`, `atomic`, `vec2`, `vec3` or
`vec4`. An `atomic` buffer holds whole numbers that shaders change with
`atomicAdd`, `atomicMax` and the rest, safely from every invocation at once: a
counter, a histogram. From a script it reads and writes like `u32`. For a
struct, give its fields instead:

```lua
local parts = gpu.buffer{ count = 100000, layout = { "pos: vec3", "vel: vec3", "life: f32" } }
```

In the shader the buffer is an array under the name you bind it to,
`parts[i].pos`. The layout follows WGSL's storage rules: a `vec3` takes 16
bytes and a struct is padded to its largest field. `buf:write` and `buf:read`
handle that for you.

`buf:write(values [, first])` takes a list of numbers, of vectors (`vec3` or
`{ x, y, z }`), or of tables with a struct's fields, or a byte string. A list
of plain numbers into a vector buffer fills the lanes in turn.

### Textures

`gpu.texture{ w = 512, h = 512 }` is an RGBA picture a shader writes. Its
`name` (`"gpu:1"`) goes anywhere a texture path goes: a material, a UI image,
`draw.quad`, a decal. What a shader writes draws as the same colours a PNG with
those bytes would.

A shader **writes** a texture with `textureStore`, and **reads** one bound with
`gpu.read(tex)` using `textureLoad`. On every platform Floptle runs on, one
texture cannot be both read and written in the same shader, so to update a
picture in place, keep two and swap them each frame (ping-pong), as the game
of life below does.

### Writing the shader

A compute shader is a `.wgsl` file in your project, with one or more
`@compute` entry points. It is plain WGSL: loops, workgroup memory, barriers,
and atomics (in workgroup memory, and in an `atomic` buffer) are all there. The engine writes the declarations in front of
it, so you never count binding numbers:

- every buffer and texture you bind, as a global under its bind name;
- `params`, holding `params.time` (seconds of play), `params.frame`, and each
  number you pass in `params = { … }` (as an `f32`);
- the noise, hash and colour functions FLSL has: `flsl_hash2`, `flsl_fbm2`,
  `flsl_remap1`, `flsl_rotate2d`, and the rest.

One file can hold several entry points (`seed`, `step`, `clear`), and each
call binds only what its entry point uses: the others are left out of that
run. A helper function a run calls has the same bindings as the run, so pass
every name it touches.

A mistake is reported in the Console at the file's own line
(`shaders/life.wgsl:12: …`), and the file is read again when you save it.

### Running it

```lua
compute.run(path, entry, {
  size = { w, h },          -- invocations wanted; or groups = { gx, gy }
  bind = { cells = cells, skin = skin, before = gpu.read(old) },
  params = { rate = 0.5 },
})
```

`size` is how many invocations you want: the engine divides it by the entry
point's `@workgroup_size` and rounds up, so check the edge in the shader when
your size is not a multiple of it. `groups` gives the workgroup counts
yourself. Calls run in the order you made them, each after the last finished,
and each appears in `perf.gpu()` as `compute <entry>`.

### Limits, and where it runs

Compute runs on every platform Floptle ships to: Windows, macOS, Linux and the
browser. `gpu.limits()` reports what this device allows. The browser's are the
tightest (8 storage buffers per shader, 256 invocations per workgroup, 16 KB of
workgroup memory), so a game that stays within them runs everywhere.

Results can differ slightly between graphics cards, and a dedicated server has
no GPU (there `gpu.buffer` answers `nil, "no renderer"`). Use compute for what
players see, not for gameplay state a multiplayer match has to agree on.

Everything you make is let go on Stop and on a scene load.

### Example: the game of life

```lua
-- scripts/life.lua
local N = 256
local a, b

function start(node)
  a = gpu.texture{ w = N, h = N }
  b = gpu.texture{ w = N, h = N }
  compute.run("shaders/life.wgsl", "seed", { size = { N, N }, bind = { out = a } })
end

function update(node, dt)
  compute.run("shaders/life.wgsl", "step", { size = { N, N }, bind = { before = gpu.read(a), out = b } })
  a, b = b, a
  node:setMaterial{ texture = a.name }
end
```

```wgsl
// shaders/life.wgsl
@compute @workgroup_size(8, 8)
fn seed(@builtin(global_invocation_id) id: vec3<u32>) {
  let alive = flsl_hash2(vec2<f32>(id.xy)) > 0.7;
  textureStore(out, vec2<i32>(id.xy), select(vec4<f32>(0.0, 0.0, 0.0, 1.0), vec4<f32>(1.0), alive));
}

@compute @workgroup_size(8, 8)
fn step(@builtin(global_invocation_id) id: vec3<u32>) {
  let size = vec2<i32>(textureDimensions(before));
  let p = vec2<i32>(id.xy);
  var n = 0;
  for (var y = -1; y <= 1; y++) {
    for (var x = -1; x <= 1; x++) {
      if (x != 0 || y != 0) {
        let q = (p + vec2<i32>(x, y) + size) % size;
        n += i32(textureLoad(before, q, 0).r > 0.5);
      }
    }
  }
  let was = textureLoad(before, p, 0).r > 0.5;
  let alive = n == 3 || (was && n == 2);
  textureStore(out, p, select(vec4<f32>(0.0, 0.0, 0.0, 1.0), vec4<f32>(1.0), alive));
}
```

### Example: a particle swarm

A hundred thousand points moved on the GPU and splatted into a texture.

```lua
-- scripts/swarm.lua
local COUNT, SIDE = 100000, 512
local parts, canvas

function start(node)
  parts = gpu.buffer{ count = COUNT, layout = { "pos: vec2", "vel: vec2" } }
  canvas = gpu.texture{ w = SIDE, h = SIDE }
  compute.run("shaders/swarm.wgsl", "seed", { size = { COUNT }, bind = { parts = parts } })
end

function update(node, dt)
  compute.run("shaders/swarm.wgsl", "clear", { size = { SIDE, SIDE }, bind = { canvas = canvas } })
  compute.run("shaders/swarm.wgsl", "advance", { size = { COUNT }, bind = { parts = parts, canvas = canvas }, params = { dt = dt } })
  node:setMaterial{ texture = canvas.name }
end
```

```wgsl
// shaders/swarm.wgsl
@compute @workgroup_size(64)
fn seed(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x >= arrayLength(&parts)) { return; }
  let f = f32(id.x);
  parts[id.x].pos = vec2<f32>(flsl_hash2(vec2<f32>(f, 1.0)), flsl_hash2(vec2<f32>(f, 2.0)));
  parts[id.x].vel = vec2<f32>(0.0);
}

@compute @workgroup_size(8, 8)
fn clear(@builtin(global_invocation_id) id: vec3<u32>) {
  textureStore(canvas, vec2<i32>(id.xy), vec4<f32>(0.0, 0.0, 0.0, 1.0));
}

@compute @workgroup_size(64)
fn advance(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x >= arrayLength(&parts)) { return; }
  var p = parts[id.x];
  // Swirl round the middle, drifting with time.
  let d = p.pos - vec2<f32>(0.5);
  let swirl = vec2<f32>(-d.y, d.x) - d * 0.2;
  p.vel = p.vel * 0.98 + swirl * params.dt;
  p.pos = fract(p.pos + p.vel * params.dt + 1.0);
  parts[id.x] = p;
  let size = vec2<f32>(textureDimensions(canvas));
  textureStore(canvas, vec2<i32>(p.pos * size), vec4<f32>(1.0, 0.6, 0.2, 1.0));
}
```

For a step-by-step start, the tutorial
[Compute shaders — a pond the GPU simulates](../tutorials/compute.md) builds a
rippling pond from an empty scene. The reference for every call is under
[compute in the API reference](../lua-api.md#compute--gpu--computerun).
