# Cameras & Dialogue (`floptle-core` cameras · `floptle-ui` dialogue)

Two subsystems that constantly work together. **Cameras**: a lightweight
virtual-camera + brain model (Cinemachine-ish) so first-person, third-person, and
cutscene shots are data you place, and blends between them are one call.
**Dialogue**: a customizable-but-good-default, typewriter system built on the UI
that can drive cutscenes, events, and scripts mid-line.

> Reads on: [ADR-0003 Lua scripting](../decisions/0003-scripting-lua.md) ·
> [ADR-0005 ECS/Node facade](../decisions/0005-scene-model-ecs-node-hybrid.md).
> Sits beside: [`./ui.md`](./ui.md) (dialogue box *is* a UI subtree) ·
> [`./input.md`](./input.md) (dialogue pushes an input context to eat advance/skip)
> · [`./scene-and-nodes.md`](./scene-and-nodes.md) · [`./physics.md`](./physics.md)
> (third-person collision uses `spherecast`). Frame: [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §3.

---

# Cameras

## The model: virtual cameras + a brain

Real GPU camera state (the view/projection the renderer uses) is owned by **one**
output camera. Authors never move it directly. Instead they place **virtual
cameras** (vcams) — cheap pose-and-lens descriptions — and a **brain** picks the
highest-priority *live* vcam and **blends** the output toward it. Switching shots,
or going gameplay→cutscene→gameplay, is just changing which vcam is live.

```
  vcam: FP_Player   (prio 10, live) ─┐
  vcam: TP_Orbit    (prio  5)        ├─▶ CameraBrain ─▶ blend(curve, t) ─▶ Output Camera ─▶ renderer
  vcam: Cut_Reveal  (prio  0)        ┘        ▲
                                  highest live vcam wins; brain eases between them
```

```rust
struct VirtualCamera {              // a node component (ADR-0005); transform = its pose
    priority: i32,                  // brain picks the highest LIVE vcam
    live: bool,                     // activated by script/event/trigger
    lens: Lens,                     // fov, near/far, optional ortho
    body: CameraBody,               // how its transform is driven (below)
    blend_in: Blend,                // curve+seconds used when THIS vcam becomes active
}

enum CameraBody {
    Fixed,                          // a static cutscene cam — pose = the node's transform
    FirstPerson { pitch_limit: f32 },          // pinned to a head node; look = "Look" axis
    ThirdPerson(Orbit),             // orbit + spring-arm collision (below)
    Dolly { track: TrackRef, speed: f32 },     // rides a spline; for sweeping reveals
}

struct Blend { curve: BlendCurve, secs: f32 }  // Cut | Linear | EaseInOut | Custom(Curve)
```

The **brain** runs late in the variable update (after gameplay moved nodes,
before render). Each frame: find the highest-priority live vcam; if it changed,
start a blend using the *incoming* vcam's `blend_in`; evaluate every live-or-
blending vcam's body to a pose; lerp position + slerp rotation + lerp lens along
the blend curve; write the result to the output camera.

```rust
impl CameraBrain {
    fn activate(&mut self, vcam: VcamId);                 // make live (uses its blend_in)
    fn blend_to(&mut self, vcam: VcamId, b: Blend);       // explicit one-off blend
    fn play_track(&mut self, vcam: VcamId, track: TrackRef); // run a Dolly shot, returns on end
}
```

## First-person & third-person rigs

- **First-person** is a `FirstPerson` vcam parented to the character's head node;
  yaw rotates the body, pitch rotates the vcam (clamped to `pitch_limit`). The
  `"Look"` axis ([`./input.md`](./input.md)) drives it; no extra wiring.
- **Third-person** is a `ThirdPerson(Orbit)`: a target + boom that orbits on
  `"Look"`, with a **spring arm** that prevents wall-clipping by casting from
  target to desired camera and pulling in on a hit — reusing the SDF
  `spherecast` from [`./physics.md`](./physics.md), so it collides correctly even
  with morphing fractal geometry.

```rust
struct Orbit {
    target: NodeRef, distance: f32, height: f32,
    yaw_pitch: Vec2, pitch_limit: (f32, f32),
    spring_arm: f32,        // collider radius for the spherecast; 0 = no collision
    follow_damp: f32, look_damp: f32,   // smoothing
}
```

## Gameplay ↔ cutscene as a blend

A cutscene is just higher-priority vcams becoming live (often `Fixed` or `Dolly`),
then handing priority back. Because the *transition is a blend*, the camera eases
out of gameplay and back without a hard cut — the player never teleports.

```lua
camera.activate("Cut_Reveal", { blend = "EaseInOut", secs = 0.8 })  -- ease into the shot
camera.play_track("Cut_Sweep", "tracks/reveal.ron")                 -- dolly along a spline
camera.release("Cut_Reveal")   -- highest remaining live vcam (FP_Player) blends back in
```

`play_track` returns/raises an event when the dolly reaches the spline end, so a
cutscene script can sequence shots. Tracks are RON splines (control points + ease).

## Camera editor UX

In the scene/UI workspace (egui, dark/retro — ADR-0004):

- **Place** vcams like any node; a frustum gizmo shows fov/near/far.
- A **"Look through"** toggle pipes a vcam straight to the viewport so you frame
  the shot live; a **priority/Solo** column shows which vcam *would* be live.
- **Dolly tracks** are editable splines in the scene view (drag control points;
  scrub a playhead to preview the move) — the same timeline feel as VFX.
- A small **Blend** field per vcam (curve + seconds); that's the whole transition.

---

# Dialogue

A built-in dialogue system with a **strong default** look that every game can
**re-skin** — never the same box twice unless you want it. It's a UI subtree
([`./ui.md`](./ui.md)) plus a runtime **player** that reads a Dialogue asset.

## Required behaviors

1. **Types out over time** — characters reveal at a per-line speed.
2. **Per-character voice SFX** — a "blip" plays as glyphs appear (skippable
   chars like spaces don't blip), pitched/picked per **speaker**.
3. **Skip the typing** — the advance input while still typing **completes the
   line instantly** (shows all text) rather than advancing.
4. **Advance again** — once fully typed, the *same* input advances to the next
   line. (One button: first press completes, second press advances.)
5. **Weave with the world** — a line can carry **embedded events** that fire
   scripts, cameras, VFX, or set flags *mid-line*.

## The Dialogue asset

A small graph: speakers, lines, optional branches/choices, embedded events. RON,
hot-reloadable like everything (ARCHITECTURE §8).

```rust
struct Dialogue {
    speakers: Vec<Speaker>,         // name, color, voice (blip SFX + pitch range)
    style: StyleRef,                // UI style for the box (re-skin here) — see ./ui.md
    start: NodeId,
    nodes: Vec<DiaNode>,
}

struct Speaker { name: String, color: Rgba, voice: VoiceRef, portrait: Option<AssetId> }

enum DiaNode {
    Line {
        id: NodeId, speaker: SpeakerId,
        text: String,
        speed: Option<f32>,        // chars/sec; None = speaker/global default
        events: Vec<Embedded>,     // fire as the playhead passes a char index
        next: NodeId,
    },
    Choice { id: NodeId, prompt: String, options: Vec<(String, NodeId)> },  // basic branch
    End { id: NodeId },
}

struct Embedded { at_char: u32, action: EventAction }   // fire when reveal reaches at_char

enum EventAction {
    Script { fn_name: String, arg: String },   // call a node-script function
    Camera { op: CamOp },                       // activate/blend/play_track a vcam
    Vfx    { effect: String },                  // vfx.play (see ./particles-vfx.md)
    Flag   { key: String, value: bool },        // set a story flag
    Pause  { secs: f32 },                       // hold the reveal (dramatic beat)
}
```

`at_char` is the magic that **weaves dialogue with cutscenes**: an event fires the
instant the typewriter reaches a glyph, so a camera can cut *on a word*.

## Runtime player

A small state machine driving the UI box; one per active conversation (usually
one). On `start`, it **pushes the `dialogue` input context** (Consume — see
[`./input.md`](./input.md)) so the world stops hearing `Advance`/`Skip`.

```
 start ─▶ Typing ──(reveal complete)──▶ Full ──(Advance)──▶ next Line / Choice / End
            ▲  │                                                          │
            │  └─(Advance while Typing = Skip)─▶ reveal all, go to Full   │
            └──────────────────── next Line ───────────────────────────◀─┘
```

Per frame while `Typing`: advance a char accumulator by `speed*dt`; for each newly
revealed glyph, play the speaker's voice blip and fire any `Embedded` event whose
`at_char` was crossed (half-open interval, like the VFX timeline — no double-fire,
none missed). `Skip` jumps the accumulator to the end **and still fires** all
pending events so cutscene cues aren't lost. On `End`, pop the input context and
raise `DialogueEnded`.

The box is the UI subtree from [`./ui.md`](./ui.md); swapping its `StyleRef`
(font, 9-slice frame, colors, portrait slot) re-skins dialogue per game — the
default ships good-looking, not generic.

## RON example — a line that cuts to a camera mid-sentence

```ron
Dialogue(
    speakers: [
        Speaker(name: "Mara", color: (0.8,0.9,1.0,1.0), voice: "sfx/voice_mara.ron"),
        Speaker(name: "???",  color: (1.0,0.4,0.4,1.0), voice: "sfx/voice_low.ron"),
    ],
    style: "DialogueRetro",
    start: 0,
    nodes: [
        Line(
            id: 0, speaker: 0,
            text: "Wait... do you see that up on the ridge?",
            speed: Some(36.0),
            events: [
                // when the reveal hits "the ridge", cut to a cutscene cam and play VFX
                Embedded(at_char: 22, action: Camera(op: Activate(vcam: "Cut_Ridge", blend: EaseInOut, secs: 0.6))),
                Embedded(at_char: 22, action: Vfx(effect: "DistantFlash")),
                Embedded(at_char: 40, action: Pause(secs: 0.5)),     // beat before the next word
            ],
            next: 1,
        ),
        Line(
            id: 1, speaker: 1,
            text: "You were not meant to find this place.",
            events: [ Embedded(at_char: 0, action: Camera(op: PlayTrack(vcam: "Cut_Ridge", track: "tracks/ridge_push.ron"))) ],
            next: 2,
        ),
        Choice(id: 2, prompt: "What do you do?", options: [
            ("Stand your ground", 3),
            ("Back away slowly",  4),
        ]),
        End(id: 3), End(id: 4),
    ],
)
```

Mid-line, the camera blends to `Cut_Ridge` and a flash VFX fires; the next line
pushes the dolly in; then a basic two-option branch. When the conversation ends,
`camera.release` (from the closing line's event or the script below) blends back
to gameplay.

## Scripting API (Lua)

```lua
-- start a conversation; control returns immediately
dialogue.start("dialogue/ridge.ron")

-- react to its events anywhere (curated event stream, ARCHITECTURE §7)
function on_event(ev)
    if ev.kind == "DialogueEvent" and ev.flag == "saw_intruder" then
        quest:advance("the_ridge")
    end
    if ev.kind == "DialogueEnded" then
        camera.release("Cut_Ridge")          -- ease back to the player camera
    end
end
```

`Embedded` `Script` actions call `fn_name(arg)` on the node script that owns the
dialogue, so a designer wires "on this word, do this" without leaving the asset.
Advance/skip are not polled by the game — the player reads the `dialogue` input
context's `Advance`/`Skip` actions itself, so the world genuinely can't act while
text is up.

## Out of scope

- **A full localization pipeline** (string tables, locale switching, font
  fallback per language). Text is inline today; **noted as future** — the asset
  already isolates strings, so a table layer can slot in.
- **A full branching-narrative visual editor** at launch. **Basic branches**
  (`Choice` nodes) ship and are editable as a list; a graph-node story editor is
  later.
- **Lip-sync / facial animation** and **localized voice-over** — voice here is
  per-speaker typewriter blips, not recorded VO.
