# Camera stacks: one camera drawn over another

A camera's **stack** is a list of other cameras drawn on top of its picture,
bottom first. Each stacked camera draws only the layers it renders, and it
draws them over an empty depth buffer, so nothing the camera beneath drew can
hide them.

The usual reason for a stack is first-person arms. Arms that are part of the world
push into every wall the player stands against. Arms filmed by a stacked camera
never touch the wall, because that camera never draws it.

## First-person arms, step by step

1. **Put the arms on a layer of their own.** Add an `Arms` layer in
   **Settings ▸ Layers** and set it on every node of the arm rig that draws
   something. Each node has its own layer, so a model child does not take its
   parent's.
2. **Take that layer out of the main camera.** On the player's camera, open
   **renders:** and untick `Arms`, so the main camera no longer draws them.
3. **Add a second camera for the arms.** Parent it to the main camera at zero
   offset, so it moves with the player's view. Under **renders:**, tick only
   `Arms`. It does not need to be the active camera.
4. **Stack it.** On the main camera, under **stack**, pick the arms camera from
   **+ add camera**, or drag it in from the Hierarchy.

Step 4 works from a script too, once the layers are set:

```lua
function start(node)
  find('ArmsCam'):setCamera{ fovY = 0.9 }
  find('PlayerCam'):setCamera{ stack = { 'ArmsCam' } }
end
```

The stacked camera has its own **field of view**. Shooters usually give the
arms a narrower one than the world, so the arms don't stretch at the edges of
a wide view.

## Order

Rows draw bottom of the list last, so the bottom row is on top. Drag a row by
its **≣** handle to reorder. In a script, `stack = { 'Arms', 'Scope' }` draws
the scope over the arms. Setting `stack` replaces the whole list, and
`stack = {}` clears it.

## What a stacked camera draws

* **Its own layers, lit by the scene.** The arms get the same sun, lamps and fog
  as the world around them, and cast sun shadows onto themselves. The sun's
  shadow map in a stacked camera holds only what that camera draws, so a wall
  shades the arms through terrain and shadow-proxy shadows but not through the
  map.
* **No sky, terrain, particles or script shapes.** The camera beneath has
  already drawn all of those, and drawing them again would cover the world.
* **The base camera's framing.** The stacked camera renders at the same size and
  aspect ratio as the picture it is laid over. Its position, rotation and field
  of view are its own.
* **Before post-processing.** Bloom, tonemapping, grading and your `stage post`
  shaders apply to the whole picture at once. Depth-based effects (ambient
  occlusion, depth of field) read the base camera's depth.

Wherever the stacked camera drew nothing, the picture underneath shows through
unchanged.

## Where it applies

Every view of the game uses the stack: the Game tab (docked or fullscreen), an
exported build, `floptle shot` and `camera.capture`. A camera with a
[render target](render-targets.md) carries its own stack into its texture, so a
scope's feed can have a reticle camera on top.

## Things to know

* **One level deep.** If a stacked camera has a stack of its own, that stack is
  not drawn, and the Inspector says so. This also means two cameras that stack
  each other cannot loop.
* **A name that matches no camera draws nothing.** The Inspector shows a warning
  on the row, and the Console reports it once.
* **Switching a stacked camera off hides what it films.** Disable the arms
  camera during a cutscene and the arms are gone, with no warning, because
  that's a normal thing to do.
* **Every stacked camera is another render** of its layers. A pair of arms costs
  little. A stacked camera that renders every layer redraws the whole scene.
