# Fofighter Sample Kit

Characters, music, a display face and a procedural sky, taken out of
**Fofighter** and put into the public domain.

There is a gap between installing an engine and having anything on screen, and
it is usually filled by a grey capsule. This is the other thing you could put
there: five models that read as characters, four tracks that set a mood, a face
with some personality in it, and a sky that does something.

**Everything here is [CC0](https://creativecommons.org/publicdomain/zero/1.0/).**
No attribution, no conditions, commercial use fine. Ship it, cut it up, sell it,
claim you made it. If crediting Fopull is easy then it is appreciated, and it is
not a requirement of any kind.

## What's in it

**Models** — `assets/models/`

| | |
|---|---|
| `Elvira.glb` | 2 parts |
| `Elle.glb` | 2 parts |
| `Sae.glb` | rigged, 17 parts |
| `SaesRapier.glb` | a prop, sized to Sae's hand |
| `DisgracedJester.glb` | authored **lying down**, in a defeated pose — that is the asset, not a broken import |

All are stylised and low-poly, roughly 8 units tall for a character, with
base-colour textures baked in.

**Music** — `assets/audio/music/`, four Ogg Vorbis tracks: `metal_fight_song`
(the fight theme), `Menu` (a loop), `ImportantStorybeatSong` and
`TenseExposition`.

**Font** — `assets/fonts/Fofighter.ttf`, the game's display face. A blocky
pixel-grid face; it has the Latin alphabet, digits and common punctuation, and
not much beyond that, so check it against your own strings before committing to
it for body text.

**Sky shader** — `assets/shaders/ashfall.flsl`, a Sky-stage shader driven
entirely by one uniform, `burn`, from 0 to 1. Point a Skybox node at it and drag
that one number to move through five phases: a cold void, embers low down, a
gothic skyline appearing in its own firelight, flames taking the roofs, and the
whole sky alight. `time` only does local churn, so a paused story leaves the sky
alive but stops it progressing.

## Using it

Install through **📦 Packages ▸ 🌐 Browse**, and the contents appear in your
Assets tab under the package's name. Nothing here runs code — there is no editor
extension and no game script in this package, so installing it cannot do
anything to your project except add files to it.

Every picture in this listing — the character shots, the sky across its five
phases, the type specimen — was rendered by the engine rather than screenshotted
by hand. That tooling currently lives in the engine's own source tree rather than
in the editor you installed, so it is not something you can run today; it is
being pointed at from here because a listing with no pictures is a listing nobody
installs, and taking them by hand is why most packages have none.
