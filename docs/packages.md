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

A package declares what it may reach for, and the list is shown before it is
installed. This is not decoration: an undeclared capability is **absent** from
the package's Lua, not merely refused when it is called.

| Permission | Gives the package |
| --- | --- |
| `Network` | `http.*` — talking to a server, and the loopback listener a browser sign-in needs |
| `Files` | reading and writing anywhere in the project (its own folder it can always read) |
| `Browser` | `ed.openUrl` / `sys.openUrl` — opening a page in your browser |

Nothing declared means the package can read its own folder and nothing else.

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
