# Packages

A package is a folder of things you can drop into any Floptle project: editor
tools, scripts, art, prefabs, scenes, shaders — or all of them at once. Anybody
can write one, and installing one is picking a folder, pasting a repository URL,
or clicking Install in the browser.

Open **Window ⏵ 📦 Packages**.

---

## Installing one

| From | What happens |
| --- | --- |
| **a folder** | copied into `<project>/packages/<id>/`, so a teammate who clones the project gets it too |
| **a repository** | cloned, then copied in exactly the same way. Needs Git on your PATH |
| **the browser** | the catalogue on fopull.com; Install does the repository route for you |
| **a link** | *not* copied — read where it lives. This is how you work on one |

Everything installed is recorded in `packages.ron` at the project root, which is
worth committing: it says which packages the project needs and where each one
came from.

A package can be **switched off** without removing it — the checkbox beside its
name. That is the first thing to try when something in the editor starts
behaving oddly, and it never deletes anything.

---

## Writing one

**Window ⏵ 📦 Packages ⏵ ✚ Add ⏵ New Package.** Give it an id and a name and you
get a folder that already works:

```text
packages/com.you.yourtool/
  package.ron       what it is, what it needs, what it may reach for
  editor/main.lua   runs in the editor
  scripts/          Lua your game can attach to nodes
  assets/           meshes, textures, prefabs, scenes, effects, shaders
  README.md
```

Press **⟲ Reload all** after an edit and your changes are live.

To develop a package that lives outside a project — one you intend to share —
put it anywhere on disk and use **🔗 Link folder** from each project you want to
try it in. Nothing is copied, so one edit reaches every project at once.

### The manifest

Only `id`, `name` and `version` are required.

```ron
(
    id: "com.you.grasstools",
    name: "Grass Tools",
    version: "1.2.0",
    description: "Scatter and paint grass on terrain.",
    author: (name: "You", url: "https://example.com"),
    license: "MIT",
    keywords: ["terrain", "vegetation"],

    // Which engine versions this works with. Leave it out and it claims all of
    // them, which is a claim worth making on purpose.
    engine: ">=0.55.0",

    // Other packages this one needs.
    dependencies: [ (id: "com.you.core", version: "^1.0") ],

    // What it ships. These are the defaults; name them only to change them.
    editor: ["editor"],
    scripts: ["scripts"],
    assets: ["assets"],

    // Optional extras, copied into the project when somebody asks for them.
    samples: [ (name: "Demo", path: "samples/demo", description: "A field") ],

    // What it may reach for. See below.
    permissions: [Network, Browser],
)
```

**The id is the identity.** Reverse-DNS, at least two parts, lowercase. It is
what a dependency names, what `pkg://` addresses resolve through, and what tells
your `grass` apart from somebody else's.

**Versions and ranges.** A version is `major.minor.patch`, optionally with a
pre-release tag (`1.0.0-rc.1`). A *range* is what a dependency or `engine` asks
for:

| Written | Means |
| --- | --- |
| `1.2.3` | 1.2.3 or any later compatible release — the same as `^1.2.3` |
| `=1.2.3` | exactly 1.2.3 |
| `^1.2.3` | ≥ 1.2.3, below the next breaking release (2.0.0; below 1.0 the caret narrows) |
| `~1.2.3` | ≥ 1.2.3, below 1.3.0 |
| `>=1.2, <1.5` | both, joined |
| `*` | anything released |

A pre-release only ever satisfies a range that names one, so `>=1.0.0` will not
quietly install `2.0.0-alpha`.

### Permissions

A package declares what it may reach for. This is not decoration: an undeclared
capability is **absent** from the package's Lua, not merely refused when it is
called.

**A package installed from a Git remote — the catalogue, or a URL somebody sent
you — does not run until you allow it.** If it declares any permission it arrives
installed but switched **off**, with what it asked for on its row in 📦 Packages,
and it starts running when you tick the box. Enabling a package runs its code, so
that tick is the decision, and it should be made having read the list.

A package that declares nothing is enabled on arrival: it can read its own folder
and nothing else, which is the standing every built-in tool already has. A
confirmation nobody can act on only teaches people to click through the ones that
matter.

| Permission | Gives the package |
| --- | --- |
| `Network` | `http.*` — talking to a server, and the loopback listener a browser sign-in needs |
| `Files` | reading and writing anywhere in the project (its own folder it can always read) |
| `Browser` | `ed.openUrl` / `sys.openUrl` — opening a page in your browser |

Listing on the catalogue is automatic once a submission passes its checks, so the
moment you tick that box is the last point at which anybody looks at what a
package wants. Read the list.

---

## Who made this, and was it any good

**Packages are made and managed by their authors, not by Fopull.** Listing on the
catalogue is automatic: a submission that passes its structural checks goes live,
and nobody has vouched for what the package does. A package is code that runs in
your editor. Trust them at your own discretion — and read the reviews.

Reviews are the thing that replaces an approval nobody gave. A package's row in
**🌐 Browse** shows its score and how many reviews it is made of, or *no reviews
yet* — which is a different statement from a bad score, and drawn differently.
Open **reviews** on a row to read them. Each says which **version** it was written
against, because a review of 1.0.0 is not a review of 3.0.0.

### Writing one

You can review a package once you have it **installed and enabled** in a project.
That is the whole gate, and it is deliberate: a package sitting there switched off
has not been tried, and since v0.55.3 that is exactly how a package that asks for
a permission arrives.

You need to be signed in. **It is the same account as the Hub** — one entry in
your operating system's keyring, shared by the Hub, the editor and every game, so
signing in anywhere signs you in everywhere. The Packages window shows who you
are, and lets you sign out or sign in as somebody else.

Pick a rating out of five; words are optional. The version you have installed goes
with it. Posting again replaces your earlier review rather than adding a second
one — you are allowed to change your mind.

Pointing the catalogue at your own registry gets you that registry's reviews too.
Posting is only ever to fopull.com, though: the editor will not send your access
token to another host.

---

## Addressing a package's files

`pkg://<id>/<path>` finds a file in a package wherever that package happens to
be — copied into the project, linked to a working copy on another disk, or
shipped inside an exported build.

```lua
node.material.texture = "pkg://com.you.grasstools/assets/blade.png"
```

Use it anywhere the engine takes an asset path.

---

## What a package can hold

**Editor tools.** `editor/*.lua` runs in the editor: menus, panels, Scene-view
overlays, world-space handles, scene edits with real undo. See
[editor-scripting.md](editor-scripting.md).

**Game scripts.** `scripts/*.lua` can be attached to nodes exactly like the
project's own. The project's `scripts/` folder is searched first, so installing a
package can never change what an existing script name means.

**Assets.** Anything the engine reads: models, textures, prefabs, scenes,
`.vfx.ron` effects, `.flsl` shaders, tilesets, UI styles.

**Samples.** Extras that are *not* loaded — example scenes, a demo project, art
nobody needs unless they ask. They are copied into `<project>/samples/` on
request, so a package can carry a hundred megabytes of demo without costing every
project that installs it.

An assets-only package is a perfectly good package. It needs no Lua at all.

---

## When one goes wrong

A package that will not load is **skipped, loudly** — the project still opens,
the Console says what happened, and the row in 📦 Packages carries the reason.
Same for a script that raises: the callback that raised stops being called rather
than raising sixty times a second, and its panel says so instead of drawing.

Common ones:

- **"needs Floptle x.y.z and this is …"** — the package's `engine` range does not
  include this build. Update the engine, or the package.
- **"needs `com.x.y` …, which is not installed"** — a dependency is missing.
  Install it; load order is worked out for you.
- **"is in a dependency cycle"** — two packages need each other. One of them has
  to stop.
- **"is listed as 1.0.0 but is really 1.1.0"** — you edited a linked package's
  version. Harmless; the package's own manifest wins.

---

## Publishing one

Put the package in a public Git repository — with `package.ron` at the root, or
in a single subfolder — and anybody can install it from the URL today.

To have it appear in **🌐 Browse**, submit it to the catalogue at
[fopull.com/packages](https://fopull.com/packages). A catalogue entry names a
repository and a revision per published version, so installing from the browser
and installing from the URL are the same operation. A studio that would rather
not publish can point the browser at its own catalogue instead — the URL is a
box under **Registry**.

---

## See also

- [editor-scripting.md](editor-scripting.md) — the whole editor API a package gets
- [scripting.md](scripting.md) — the Lua a game's own scripts use
- [export-builds.md](export-builds.md) — packages travel with an exported build
