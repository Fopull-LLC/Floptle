//! **The command line, as data** — ADR-0027.
//!
//! One table, [`VERBS`], is the source of truth. The parser is generated from
//! it, `--help` is generated from it, and so is `floptle help --json`, which
//! publishes the whole surface to a caller that cannot read this file.
//!
//! ## Why a table rather than a derive
//!
//! `clap`'s derive models *how to parse this*. It has nowhere to put the things
//! an automated caller most needs to know before it runs anything: does this
//! verb need a GPU, does it write into my project, what comes back, and what
//! does each exit code mean. That metadata would have to live somewhere anyway,
//! and two descriptions of one command line is exactly the failure this replaces
//! — three hand-rolled parsers that disagreed about unknown arguments, about
//! `--flag=value`, and about what exits with what.
//!
//! So the table carries all of it, `clap`'s **builder** does the parsing, and
//! neither can drift from the other because only one of them is written down.
//!
//! ## What this module does not do
//!
//! It does not implement any verb. Every arm calls the function the editor's
//! own menu item calls — `new_project`, `migrate_project`,
//! `export::headless_export`, `anim::extract_clips`. That is the rule
//! `docs/export-builds.md` set for the export path and it generalises: a command
//! line that reimplements a panel drifts from it, and one that calls the same
//! function cannot.
//!
//! ## The old flags still work
//!
//! `--export`, `--new`, `--migrate`, `--version` and the rest are a shipped
//! interface with callers nobody here controls — the Hub, CI, anybody's scripts.
//! [`dispatch`] only claims a command line whose first argument is a **verb**;
//! everything else falls through to the original loop in `main.rs`, untouched.
//! One consequence worth knowing: a directory named `check` is shadowed by the
//! verb of that name, and `floptle open check` is the way to say the directory.

use std::path::{Path, PathBuf};

/// What one argument takes.
pub(crate) enum Value {
    /// Present or absent, no value.
    Flag,
    /// A filesystem path.
    Path,
    /// Free text.
    Text,
    /// One of a fixed set, which `--help` lists, the parser enforces and
    /// `help --json` publishes.
    ///
    /// A function rather than a slice because every set worth having is owned
    /// somewhere else — the export platforms are the distribution's list, the
    /// starter projects are the template table — and copying one here is how
    /// a `--help` comes to offer a value that no longer exists, or to omit one
    /// that does.
    Choice(fn() -> Vec<String>),
}

/// Everything `export` will stamp for.
fn export_platforms() -> Vec<String> {
    let mut v = vec!["host".to_string()];
    v.extend(floptle_dist::PLATFORMS.iter().map(|s| (*s).to_string()));
    v.push(floptle_dist::WEB_PLATFORM.to_string());
    v
}

/// Everything `new --template` accepts.
fn template_names() -> Vec<String> {
    crate::templates::names().iter().map(|s| (*s).to_string()).collect()
}

impl Value {
    /// The name `help --json` publishes for this type.
    fn type_name(&self) -> &'static str {
        match self {
            Value::Flag => "flag",
            Value::Path => "path",
            Value::Text => "text",
            Value::Choice(_) => "choice",
        }
    }

    /// The values this argument accepts, when there is a fixed set.
    fn choices(&self) -> Option<Vec<String>> {
        match self {
            Value::Choice(set) => Some(set()),
            _ => None,
        }
    }
}

/// One argument of one verb. Positionals are named in caps by convention and
/// carry no leading dashes; options carry their long spelling including them.
pub(crate) struct Arg {
    /// `PROJECT` for a positional, `--scene` for an option.
    pub(crate) name: &'static str,
    pub(crate) value: Value,
    pub(crate) required: bool,
    pub(crate) help: &'static str,
}

impl Arg {
    fn is_option(&self) -> bool {
        self.name.starts_with("--")
    }

    /// The id `clap` knows this argument by — the name without its dashes.
    fn id(&self) -> &'static str {
        self.name.trim_start_matches('-')
    }
}

/// One verb.
///
/// `name` may contain a space (`"bake gi"`), which builds a nested subcommand
/// while keeping this table flat — a flat table is what makes `help --json` a
/// list a caller can walk rather than a tree it has to recurse.
pub(crate) struct Verb {
    pub(crate) name: &'static str,
    /// One line, shown in the top-level help.
    pub(crate) summary: &'static str,
    /// Shown by `floptle help <verb>`. Empty when the summary says it all.
    pub(crate) detail: &'static str,
    pub(crate) args: &'static [Arg],
    /// **Does this need a GPU?** An automated caller reads this to know whether
    /// the verb can run where it is — in a container, over ssh, in CI.
    pub(crate) needs_gpu: bool,
    /// **Does this write into the project directory?** A caller reads this to
    /// know whether to expect its working tree to change.
    pub(crate) writes_project: bool,
    /// Exit codes beyond the two every verb shares (see [`COMMON_EXITS`]).
    pub(crate) exits: &'static [(i32, &'static str)],
    /// What comes back, for a caller that has to parse it.
    pub(crate) output: &'static str,
    /// The flag spellings this verb replaces. They keep working; this is how
    /// `help --json` says which new verb a script should move to.
    pub(crate) legacy: &'static [&'static str],
}

impl Verb {
    /// `("bake", Some("gi"))` for a nested verb, `("new", None)` for a flat one.
    fn split(&self) -> (&'static str, Option<&'static str>) {
        match self.name.split_once(' ') {
            Some((head, tail)) => (head, Some(tail)),
            None => (self.name, None),
        }
    }
}

/// The codes every verb uses the same way. A verb that needs more says so in
/// its own row, and both halves reach `help --json` together.
pub(crate) const COMMON_EXITS: &[(i32, &str)] = &[
    (0, "the operation succeeded"),
    // Including a PROJECT that is not one. Said here rather than in each row
    // because every verb taking a project answers it the same way, and "your
    // path is wrong" must not share a code with "your project is broken".
    (2, "the command line was wrong, including a PROJECT that is not a project directory"),
];

const TEMPLATE_HELP: &str = "which starter project to write (see `floptle templates`)";
const ENGINE_VERSION_HELP: &str =
    "the engine version to stamp into the project — the Hub passes the install it chose";

/// **The command line.** Adding a verb is adding a row.
pub(crate) const VERBS: &[Verb] = &[
    Verb {
        name: "new",
        summary: "scaffold a project and exit",
        detail: "Writes the project directories, the default materials and scripts, an input \
                 map and a starter scene, then exits. Refuses to write over a directory that \
                 already holds a project.",
        args: &[
            Arg { name: "DIR", value: Value::Path, required: true, help: "where to write it" },
            Arg {
                name: "--template",
                value: Value::Choice(template_names),
                required: false,
                help: TEMPLATE_HELP,
            },
            Arg {
                name: "--engine-version",
                value: Value::Text,
                required: false,
                help: ENGINE_VERSION_HELP,
            },
        ],
        needs_gpu: false,
        writes_project: true,
        exits: &[(1, "the directory already holds a project, or could not be written")],
        output: "a line per thing written, on stdout",
        legacy: &["--new"],
    },
    Verb {
        name: "templates",
        summary: "print the starter projects and exit",
        detail: "",
        args: &[],
        needs_gpu: false,
        writes_project: false,
        exits: &[],
        output: "one line per template: its name and what it is",
        legacy: &["--list-templates"],
    },
    Verb {
        name: "open",
        summary: "open a project in the editor",
        detail: "The default when no verb is given. Use this form when the project directory \
                 shares a name with a verb.",
        args: &[Arg {
            name: "PROJECT",
            value: Value::Path,
            required: false,
            help: "the project directory (default: assets/)",
        }],
        needs_gpu: true,
        writes_project: false,
        exits: &[],
        output: "none — it opens a window",
        legacy: &[],
    },
    Verb {
        name: "play",
        summary: "run a project as a game, with no editor UI",
        detail: "The same build, in player mode: no docks, no gizmos, and warnings and errors \
                 go to stderr because there is no Console to read them in.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--steam",
                value: Value::Flag,
                required: false,
                help: "initialize Steam even if the project has no App ID set (Spacewar 480) — \
                       the way to try the overlay, achievements and the rest before a \
                       partner account exists. Without it, Steam activates only for a \
                       project with a Steam App ID in ⚙ Settings ▸ Game",
            },
        ],
        needs_gpu: true,
        writes_project: false,
        exits: &[],
        output: "the game's own output on stdout; warnings and errors on stderr",
        legacy: &["--play"],
    },
    Verb {
        name: "export",
        summary: "stamp a runnable build and exit",
        detail: "Runs the same code the File ▸ Export Game… dialog runs, so a scripted build \
                 gets exactly the editor's behaviour. No window and no GPU.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: true,
                help: "the project directory",
            },
            Arg { name: "OUT", value: Value::Path, required: true, help: "where to write it" },
            Arg {
                name: "PLATFORM",
                value: Value::Choice(export_platforms),
                required: true,
                help: "which platform to stamp for",
            },
            Arg {
                name: "--title",
                value: Value::Text,
                required: false,
                help: "the game's title (default: the project directory's name)",
            },
        ],
        needs_gpu: false,
        writes_project: false,
        exits: &[(1, "the build could not be stamped")],
        output: "progress on stdout; the build lands under OUT",
        legacy: &["--export"],
    },
    Verb {
        name: "migrate",
        summary: "bring a project up to this engine version and exit",
        detail: "",
        args: &[
            Arg {
                name: "DIR",
                value: Value::Path,
                required: true,
                help: "the project directory",
            },
            Arg {
                name: "--engine-version",
                value: Value::Text,
                required: false,
                help: ENGINE_VERSION_HELP,
            },
        ],
        needs_gpu: false,
        writes_project: true,
        exits: &[(1, "the project could not be migrated")],
        output: "a line per thing changed, on stdout",
        legacy: &["--migrate"],
    },
    Verb {
        name: "bake clips",
        summary: "re-bake a model's embedded animation clips and exit",
        detail: "For clips that went stale against a replaced model — extracted placeholders \
                 left animating a couple of bones while the real animation is full-body.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: true,
                help: "the project directory",
            },
            Arg {
                name: "MODEL",
                value: Value::Path,
                required: true,
                help: "the model whose clips to re-bake",
            },
        ],
        needs_gpu: false,
        writes_project: true,
        exits: &[(1, "the clips could not be extracted")],
        output: "one line per clip written, then a count",
        legacy: &["--extract-clips"],
    },
    Verb {
        name: "bake gi",
        summary: "bake a scene's light probes, and exit",
        detail: "Renders the scene's Light Probes node into baked global illumination and \
                 writes it beside the scene. No window: it runs the bake the editor runs, \
                 back to back instead of a slice per frame, so it belongs in a build \
                 pipeline.\n\n\
                 It needs a graphics adapter — the bake photographs the scene from every \
                 probe. A scene with no enabled Light Probes node has nothing to bake and \
                 says so.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--scene",
                value: Value::Text,
                required: false,
                help: "one scene, by path or by name",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: true,
        writes_project: true,
        exits: &[(
            1,
            "there is no Light Probes node to bake, the named scene is not there, or this \
             machine cannot render",
        )],
        output: "one line saying how many probes, how many bounces, how long and what it \
                 wrote; with --json an object with `ok`, `errors` and `log`",
        legacy: &["--bake-gi"],
    },
    Verb {
        name: "bake nav",
        summary: "bake a scene's navmesh, and exit",
        detail: "Works out where a character can walk and writes the `.fnav` beside the \
                 scene — the same bake the Bake button runs, with nobody watching.\n\n\
                 No graphics adapter needed: a bake is triangles and numbers, and the \
                 triangles are read straight off disk. That makes it the one to reach for \
                 on a build server, and the one to reach for when a level's geometry is \
                 generated rather than placed — a level built by a script needs its navmesh \
                 built by one too.\n\n\
                 A scene with no Nav Mesh node has nothing to bake and says so.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--scene",
                value: Value::Text,
                required: false,
                help: "one scene, by path or by name",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: false,
        writes_project: true,
        exits: &[(
            1,
            "there is no Nav Mesh node to bake, the named scene is not there, or nothing in \
             it was walkable",
        )],
        output: "the bake's own summary — polygons, square metres, triangles, seconds, and \
                 the drops and jumps it found; with --json an object with `ok`, `errors` \
                 and `log`",
        legacy: &[],
    },
    Verb {
        name: "exec",
        summary: "run a Lua script against a project, and exit",
        detail: "The editor's own extension API, headless: `scene.*` to read and write the \
                 node graph, `ed.*` to ask the editor about itself, plus `mesh.*`, `nav.*`, \
                 `json.*` and `http.*`. Everything a package can do, a script here can do — \
                 `docs/editor-scripting.md` is the reference, and a test holds it against the \
                 live bindings.\n\n\
                 Calls that need a window — a panel, a dialog, a camera move, the clipboard — \
                 are REFUSED with a line naming them, never dropped.\n\n\
                 A script you named on your own command line is your own code, so it is \
                 granted every permission a package would have to declare.",
        args: &[
            Arg {
                name: "SCRIPT",
                value: Value::Path,
                required: true,
                help: "the .lua file to run",
            },
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: false,
        writes_project: true,
        exits: &[(1, "the script raised, or something it did reported an error")],
        output: "one line per log entry, then a summary; with --json an object with \
                 `ok`, `errors`, `warnings`, `raised` and `log`",
        legacy: &[],
    },
    Verb {
        name: "shot",
        summary: "render a scene to a PNG, and exit",
        detail: "Draws one frame through the editor's own offscreen path — the same one the \
                 docked Game view, camera previews and render targets come through — so what \
                 lands in the file is what the editor would show.\n\n\
                 It looks through the scene's ACTIVE camera, or one named with --camera. A \
                 scene with no camera has no view and says so rather than inventing an angle.\n\n\
                 The scene is not played: nothing has moved and no `start` has run. That is \
                 the right frame for \"what did my edit do\"; use `run` for what happens next.\n\n\
                 The project's post-processing is applied — bloom, vignette, ambient \
                 occlusion, posterise, colour grading, depth of field, its own `stage post` \
                 shaders, and the retro presentation at its own resolution. Motion blur is \
                 not: a single frame has no previous one to smear against.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--scene",
                value: Value::Text,
                required: false,
                help: "which scene, by path or by name (default: the project's entry scene)",
            },
            Arg {
                name: "--camera",
                value: Value::Text,
                required: false,
                help: "look through this camera node instead of the active one",
            },
            Arg {
                name: "--size",
                value: Value::Text,
                required: false,
                help: "WxH in pixels, or one number for a square (default: 960x540)",
            },
            Arg {
                name: "--out",
                value: Value::Path,
                required: false,
                help: "where to write the PNG (default: <PROJECT>/<scene>.png)",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
            Arg {
                name: "--timing",
                value: Value::Flag,
                required: false,
                help: "also report what each render pass cost on the GPU, in milliseconds — \
                       the per-pass split the editor's ⏱ panel shows, without a window. Needs \
                       a device with timestamp queries, and says so when there is none",
            },
        ],
        needs_gpu: true,
        writes_project: true,
        exits: &[(1, "the scene has no camera, or the file could not be written")],
        output: "a PNG at --out; the path on stdout, then one line per GPU pass under \
                 --timing; with --json an object with `ok`, `path`, `width`, `height`, \
                 `camera` and, under --timing, `timing` (`gpu_ms` and `passes`)",
        legacy: &[],
    },
    Verb {
        name: "vfx",
        summary: "render a particle effect to PNGs across its own timeline, and exit",
        detail: "An effect is a thing that happens over time, so one frame is the wrong \
                 question: a burst reads as a blank frame before it fires and as drifting \
                 smoke after it, and both are correct. This renders a spread of moments \
                 across the effect's own span — the timeline plus however long its last \
                 particles outlive it — and says what second each picture is at, so the next \
                 run can ask for a moment between two of them with --at.\n\n\
                 Every moment is an independent deterministic scrub from t = 0, so --at 0.5 \
                 on its own gives exactly the frame a spread would put at 0.5s. The middle of \
                 an effect can be looked at without watching the start of it first.\n\n\
                 THE CAMERA IS THE SAME IN EVERY FRAME. It is framed once, on every moment at \
                 once, then held — the frames exist to be compared against each other, and \
                 two pictures at different zooms cannot be. With more than one frame they are \
                 also tiled into one contact sheet, which is the picture to look at first.\n\n\
                 With no --scene the effect is rendered alone on a bare stage: lighting, a \
                 flat background, and deliberately no ground plane or reference cube — those \
                 are things the author did not put in their effect. Pass --scene to see it in \
                 a level instead, at the node that carries it and through that scene's camera.\n\n\
                 Which moments are worth photographing is decided by LOOKING: the effect is \
                 rendered at thumbnail size across its whole timeline first, and the frames are \
                 spread over the part where something actually lands in the picture. So a burst \
                 that is over in a tenth of a second gets five frames of that tenth, not four \
                 frames of the empty second after it.\n\n\
                 Each frame reports how much of the picture the effect covers — measured off \
                 the rendered pixels, so the number can never disagree with the image beside \
                 it.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--effect",
                value: Value::Text,
                required: true,
                help: "which effect, by key (vfx/Sparks) or by name (Sparks)",
            },
            Arg {
                name: "--at",
                value: Value::Text,
                required: false,
                help: "render exactly these moments, in seconds (say 0,0.25,0.5)",
            },
            Arg {
                name: "--frames",
                value: Value::Text,
                required: false,
                help: "how many moments to spread across the effect's span (default: 5)",
            },
            Arg {
                name: "--scene",
                value: Value::Text,
                required: false,
                help: "render it inside this scene, through that scene's camera",
            },
            Arg {
                name: "--camera",
                value: Value::Text,
                required: false,
                help: "with --scene, look through this camera node instead of the active one",
            },
            Arg {
                name: "--background",
                value: Value::Text,
                required: false,
                help: "hex colour behind the effect (default: 1c1c21, a neutral dark grey)",
            },
            Arg {
                name: "--size",
                value: Value::Text,
                required: false,
                help: "WxH per frame, or one number for a square (default: 480x480)",
            },
            Arg {
                name: "--out",
                value: Value::Path,
                required: false,
                help: "directory to write the frames into (default: PROJECT)",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: true,
        writes_project: true,
        exits: &[(
            1,
            "there is no such effect, it emits nothing at any moment asked for, the scene has \
             no camera, or a file could not be written",
        )],
        output: "one PNG per moment (plus a contact sheet when there is more than one); their \
                 paths, times and live particle counts on stdout, or an object with --json",
        legacy: &[],
    },
    Verb {
        name: "run",
        summary: "run a project headlessly for a bounded time, and report what happened",
        detail: "The editor's own play loop with no window and no GPU: the scene-transition \
                 queue, script hot-reload, the frame pass, the fixed-rate tick pass and \
                 physics, all of it. Reports every warning, error and `print`, with the file \
                 and line that raised it, and says whether each happened while opening the \
                 project or while playing.\n\n\
                 Time is FIXED, never read off the clock, so two runs of one project agree. \
                 That does mean a bug which only appears at a particular frame rate will not \
                 appear here.\n\n\
                 Nothing draws and nothing is pressed: models are not registered (physics is \
                 unaffected — a mesh collider reads its triangles from the file), and every \
                 key reads as up. \"No errors\" means nothing raised, not that the game is \
                 good.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--scene",
                value: Value::Text,
                required: false,
                help: "run this scene instead of the project's entry scene",
            },
            Arg {
                name: "--frames",
                value: Value::Text,
                required: false,
                help: "how many steps to run (default: 120)",
            },
            Arg {
                name: "--seconds",
                value: Value::Text,
                required: false,
                help: "how much simulated time to run, instead of --frames",
            },
            Arg {
                name: "--seed",
                value: Value::Text,
                required: false,
                help: "pin the game's randomness — math.random and the no-seed rng() form — \
                       to this whole number, so two runs are the same run. Without it a game \
                       that re-randomises its cast or loot moves its own --timing and --alloc \
                       figures more than most changes do, and an A/B between two engine \
                       settings is noise",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
            Arg {
                name: "--alloc",
                value: Value::Flag,
                required: false,
                help: "also report how much Lua heap a frame allocates, and which scripts \
                       allocate it. Measured across a window in the middle of the run with \
                       the collector STOPPED — which is the only way it can be measured, and \
                       means the heap grows unchecked across that window, so a --timing \
                       figure from the SAME run is not representative. Needs a span of at \
                       least 12 steps, and says so when it does not have one",
            },
            Arg {
                name: "--timing",
                value: Value::Flag,
                required: false,
                help: "also report what the steps really cost, as a distribution of \
                       milliseconds — p50, p95, p99 and the worst. The span itself stays \
                       fixed; this measures the machine, not the game clock",
            },
            Arg {
                name: "--steam",
                value: Value::Flag,
                required: false,
                help: "initialize Steam (the project's app id, or Spacewar 480 if unset) and \
                       pump it once per step — off by default, since most runs (CI included) \
                       have no Steam client to talk to",
            },
        ],
        needs_gpu: false,
        // It opens the project the way the editor does, and that tops up a
        // project's seeded files (the input map, the example shaders). Said
        // here because a caller reads this field to know whether to expect its
        // working tree to change.
        writes_project: true,
        exits: &[(1, "something raised while opening or playing")],
        output: "one line per log entry, then a summary; with --json an object with \
                 `ok`, `steps`, `errors`, `warnings` and `log`, plus `timing` under \
                 --timing, `alloc` (with a per-script `by_script` list) under --alloc, and \
                 `seed` under --seed",
        legacy: &[],
    },
    Verb {
        name: "api",
        summary: "search what a script can call, and exit",
        detail: "Reads the same table the editor's Docs tab, its autocomplete and its hover \
                 docs read, and that `docs/lua-api.md` is generated from — so the answer here \
                 is the answer the tool gives.\n\n\
                 With no query it prints every name there is, grouped. With one it prints the \
                 matches best-first: an exact name, then the part after the last dot or colon, \
                 then a prefix, then anything containing it, then a match in the description.",
        args: &[
            Arg {
                name: "QUERY",
                value: Value::Text,
                required: false,
                help: "a name, part of one, or a word from its description",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: false,
        writes_project: false,
        exits: &[(1, "nothing matched — the way `grep` answers")],
        output: "the matching entries; with --json an object with `matched` and `entries`",
        legacy: &[],
    },
    Verb {
        name: "inspect",
        summary: "print what is in a project, or in one scene, and exit",
        detail: "With no options it describes the project: what it is called, what version it \
                 was stamped with, and every scene with a node count. With --scene it prints \
                 that scene's nodes as a tree. With --select it prints the nodes that match, \
                 and under --json each one carries its whole document — which is what to read \
                 before patching a scene by hand.\n\n\
                 A --select query is a name by default (case-insensitive, any part of it), or \
                 one of `id:N`, `type:Sprite`, `script:player`.\n\n\
                 It reads the FILES, not a running world: the caller is usually about to edit \
                 them.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--scene",
                value: Value::Text,
                required: false,
                help: "one scene, by path or by name",
            },
            Arg {
                name: "--select",
                value: Value::Text,
                required: false,
                help: "only nodes matching this: a name, or id:N / type:T / script:K",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: false,
        writes_project: false,
        exits: &[(
            1,
            "the project or the named scene could not be read, or --select matched nothing \
             (the way `grep` answers)",
        )],
        output: "a description on stdout; with --json an object whose shape depends on \
                 whether --scene or --select was given",
        legacy: &[],
    },
    Verb {
        name: "check",
        summary: "report anything wrong with a project, and exit",
        detail: "Loads every scene, prefab, effect and material, runs the engine's own wiring \
                 checks, and reports every reference that does not resolve to a file. No window \
                 and no GPU. A `.ron` file that parses is not a scene that works, and this is \
                 the difference.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: false,
        writes_project: false,
        exits: &[(1, "something in the project is wrong")],
        output: "one line per finding, then a count; with --json an object with \
                 `ok`, `examined`, `errors`, `warnings` and `findings`",
        legacy: &[],
    },
    Verb {
        name: "lint",
        summary: "report what a project would have to change to switch its vec3, and exit",
        detail: "Scans every script for the two things that differ between `script_vec3: Exact` \
                 and `Fast` (ADR-0028): a vec3 component being assigned, which raises in `fast` \
                 because a native vector is immutable, and a `type()` asked about a vec3, which \
                 answers \"vector\" there instead of \"userdata\" and so silently takes the \
                 other branch. Everything else about the two is identical. This is a textual \
                 scan and not a type checker: it finds the common shapes and says so rather \
                 than implying it found them all.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--vec3",
                value: Value::Flag,
                required: false,
                help: "the vec3 migration checklist (currently the only lint, and the default)",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: false,
        writes_project: false,
        exits: &[(1, "the project has something to change before switching")],
        output: "one line per finding with the fix, then a count; with --json an object with \
                 `ok`, `scanned`, `findings` and `complete`",
        legacy: &[],
    },
    Verb {
        name: "serve",
        summary: "run a project as an authoritative dedicated server, until interrupted",
        detail: "The same server `floptle-runtime --server` runs — this is where to type it, \
                 so one command line covers the engine. No window, no GPU, and it does not \
                 return: it ticks the scene and replicates it until something stops it.\n\n\
                 Give it a PORT to listen directly, or a RELAY address to be reachable from \
                 behind a router — the relay prints a lobby code for players to join with. \
                 With neither it listens on the default port.\n\n\
                 The scene needs Networked nodes, or a session would replicate nothing, and \
                 it must not be a rollback scene: a rollback match is simulated by every \
                 peer and hosted by one of the players, so there is nothing here to drive \
                 it. Both are refused by name rather than served empty.",
        args: &[
            Arg {
                name: "PROJECT",
                value: Value::Path,
                required: false,
                help: "the project directory (default: assets/)",
            },
            Arg {
                name: "--scene",
                value: Value::Text,
                required: false,
                help: "which scene to serve (default: the project's entry scene)",
            },
            Arg {
                name: "--port",
                value: Value::Text,
                required: false,
                help: "listen on this port",
            },
            Arg {
                name: "--relay",
                value: Value::Text,
                required: false,
                help: "reach players through this relay instead, and print a lobby code",
            },
            Arg {
                name: "--tick",
                value: Value::Text,
                required: false,
                help: "simulation rate in Hz (default: 60)",
            },
            Arg {
                name: "--interest",
                value: Value::Text,
                required: false,
                help: "only replicate what is within this many units of a player",
            },
            Arg {
                name: "--budget",
                value: Value::Text,
                required: false,
                help: "cap on entities replicated per tick",
            },
        ],
        needs_gpu: false,
        writes_project: false,
        exits: &[
            (
                1,
                "the scene could not be loaded, has no Networked nodes, or is a rollback \
                 scene no dedicated server can drive",
            ),
            (3, "there was nothing wrong with the request, but it could not listen — the \
                 port is taken, or the relay refused"),
        ],
        output: "a line naming what it is listening on (and the lobby code, with --relay), \
                 then a heartbeat while it runs",
        legacy: &["floptle-runtime --server"],
    },
    Verb {
        name: "doctor",
        summary: "report what this machine can run, and exit",
        detail: "`help --json` marks which commands need a display. This says whether the \
                 machine reading it has one — and it answers by BUILDING the renderer rather \
                 than by asking the adapter what it supports, because those are different \
                 questions. An OpenGL adapter exists, reports itself happily, and cannot \
                 build floptle's shaders.\n\n\
                 Exits non-zero when the machine cannot render, so a script can branch on it \
                 before reaching for `shot` or `bake gi`.",
        args: &[Arg {
            name: "--json",
            value: Value::Flag,
            required: false,
            help: "answer as JSON",
        }],
        needs_gpu: false,
        writes_project: false,
        exits: &[(1, "this machine cannot render — the commands marked `needsGpu` will not run")],
        output: "the engine version, the graphics adapter, and whether the renderer builds; \
                 with --json an object with `engine`, `adapter`, `canRender` and `whyNot`",
        legacy: &[],
    },
    Verb {
        name: "version",
        summary: "print the engine version and exit",
        detail: "",
        args: &[Arg {
            name: "--json",
            value: Value::Flag,
            required: false,
            help: "answer as JSON",
        }],
        needs_gpu: false,
        writes_project: false,
        exits: &[],
        output: "`{\"engine\":…,\"version\":…}` with --json, otherwise one line",
        legacy: &["--version", "-V"],
    },
    Verb {
        name: "help",
        summary: "describe the command line, or one verb of it",
        detail: "With --json it publishes the whole surface: every verb, every argument, every \
                 exit code, what each verb returns, whether it needs a GPU and whether it \
                 writes to your project. Read it once instead of scraping --help.",
        args: &[
            Arg {
                name: "VERB",
                value: Value::Text,
                required: false,
                help: "describe just this verb",
            },
            Arg {
                name: "--json",
                value: Value::Flag,
                required: false,
                help: "answer as JSON",
            },
        ],
        needs_gpu: false,
        writes_project: false,
        exits: &[(1, "no such verb")],
        output: "help text, or the whole table as JSON with --json",
        legacy: &["--help", "-h"],
    },
];

/// What the caller should do next.
pub(crate) enum Outcome {
    /// The command line held no verb — run the original argument loop.
    Legacy,
    /// A verb ran to completion. Exit with this code.
    Exit(i32),
    /// A verb that needs a window. The editor starts with these.
    Launch { project: Option<PathBuf>, player: bool, steam: bool },
}

/// `floptle --help`, the flag form.
///
/// Prints the verb help, then the flags those verbs replaced — because the flags
/// still work and somebody's script is built on one. Generated from the same
/// table, so the old surface cannot be documented into existence after it stops
/// being true, and the long hand-written help string this replaced cannot drift
/// from what the binary accepts.
pub(crate) fn print_help_and_flags() {
    let _ = command().print_long_help();
    println!("\n\nThe flags this tool has always taken still work:\n");
    let width = VERBS.iter().map(|v| v.legacy.join(", ").len()).max().unwrap_or(0);
    for v in VERBS {
        if v.legacy.is_empty() {
            continue;
        }
        println!("  {:<width$}  now `floptle {}`", v.legacy.join(", "), v.name);
    }
    println!(
        "\nA `floptle-game.ron` manifest next to the binary implies `play` — that is how an\n\
         exported game runs."
    );
}

/// Is `arg` the name of a verb (or the head of a nested one)?
pub(crate) fn is_verb_head(arg: &str) -> bool {
    VERBS.iter().any(|v| v.split().0 == arg)
}

/// Read the command line.
///
/// Returns [`Outcome::Legacy`] unless `args[1]` is a verb, so every invocation
/// that worked before this module existed still reaches the code that served it.
pub(crate) fn dispatch(args: &[String]) -> Outcome {
    let Some(first) = args.get(1) else { return Outcome::Legacy };
    if !is_verb_head(first) {
        return Outcome::Legacy;
    }
    // **A closed pipe is not a crash.** Rust starts every process with SIGPIPE
    // ignored, so `floptle inspect … | head` makes `println!` return EPIPE,
    // which panics — and this binary files a crash report on panic, so using
    // `head` ends with a note asking the user to report a bug. Restoring the
    // default disposition makes the process die quietly the way every other
    // command-line tool does.
    //
    // Only on the verb path, deliberately. The flags the Hub drives keep exactly
    // the behaviour they shipped with, and this binary is also a GUI: a window
    // whose stdout pipe closes should not take the editor down with it.
    sigpipe(SigPipe::Fatal);
    let m = match command().try_get_matches_from(args) {
        Ok(m) => m,
        Err(e) => {
            // clap writes its own message, including the did-you-mean that is
            // most of the reason this dependency is here. Usage errors are 2;
            // an explicit --help/--version is not an error.
            let _ = e.print();
            return Outcome::Exit(if e.use_stderr() { 2 } else { 0 });
        }
    };
    let out = run(&m);
    // **…and three verbs are not command-line tools at all.** `open`, `play` and
    // `bake gi` go on to run the whole editor, and the paragraph above says why
    // that must not die on a closed pipe — but the reset happened before the
    // parse, so it had been reaching them. `floptle play game | head -5` killed
    // the game the moment `head` left. Nothing has been printed yet on this
    // path, so putting it back here costs nothing.
    if matches!(out, Outcome::Launch { .. }) {
        sigpipe(SigPipe::Ignored);
    }
    out
}

/// What a write to a closed pipe does.
enum SigPipe {
    /// The default disposition, and what every other command-line tool has:
    /// the process dies quietly.
    Fatal,
    /// What Rust sets at startup, which turns the write into an error, which
    /// `println!` turns into a panic, which this binary turns into a crash
    /// report.
    Ignored,
}

fn sigpipe(#[allow(unused_variables)] how: SigPipe) {
    #[cfg(unix)]
    // SAFETY: setting a signal disposition on the main thread before any of the
    // editor's threads exist. `SIG_DFL` is what the process would have had if
    // Rust had not changed it at startup.
    unsafe {
        libc::signal(
            libc::SIGPIPE,
            match how {
                SigPipe::Fatal => libc::SIG_DFL,
                SigPipe::Ignored => libc::SIG_IGN,
            },
        );
    }
}

/// Build the parser from [`VERBS`].
fn command() -> clap::Command {
    use clap::{Arg as ClapArg, ArgAction, Command};

    fn arg_of(a: &'static Arg) -> ClapArg {
        let mut c = ClapArg::new(a.id()).help(a.help);
        if a.is_option() {
            c = c.long(a.id());
            match a.value {
                Value::Flag => c = c.action(ArgAction::SetTrue),
                _ => c = c.action(ArgAction::Set).value_name(a.id().to_uppercase()),
            }
        } else {
            c = c.value_name(a.name).action(ArgAction::Set);
        }
        if let Some(choices) = a.value.choices() {
            c = c.value_parser(clap::builder::PossibleValuesParser::new(choices));
        }
        c.required(a.required)
    }

    fn leaf(v: &'static Verb, name: &'static str) -> Command {
        let mut c = Command::new(name).about(v.summary);
        if !v.detail.is_empty() {
            c = c.long_about(format!("{}\n\n{}", v.summary, v.detail));
        }
        for a in v.args {
            c = c.arg(arg_of(a));
        }
        c
    }

    let mut root = Command::new("floptle")
        .about(concat!(
            "The Floptle engine.\n\n",
            "With no verb, a bare path opens that project in the editor, and the flags this \
             tool has always taken keep working. `floptle help --json` describes every verb, \
             its arguments, its exit codes and what it returns."
        ))
        .version(crate::distribution_version())
        .subcommand_required(false)
        .disable_help_subcommand(true)
        .disable_version_flag(true);

    // Flat rows, nested where a name carries a space.
    let mut nested: Vec<(&'static str, Vec<Command>)> = Vec::new();
    for v in VERBS {
        match v.split() {
            (head, None) => root = root.subcommand(leaf(v, head)),
            (head, Some(tail)) => {
                let child = leaf(v, tail);
                match nested.iter_mut().find(|(h, _)| *h == head) {
                    Some((_, kids)) => kids.push(child),
                    None => nested.push((head, vec![child])),
                }
            }
        }
    }
    for (head, kids) in nested {
        let mut group = clap::Command::new(head)
            .about(group_summary(head))
            .subcommand_required(true)
            .arg_required_else_help(true);
        for k in kids {
            group = group.subcommand(k);
        }
        root = root.subcommand(group);
    }
    root
}

/// The one-liner for a verb group, built from the children so it cannot go
/// stale when one is added.
fn group_summary(head: &str) -> String {
    let kids: Vec<&str> = VERBS
        .iter()
        .filter_map(|v| match v.split() {
            (h, Some(tail)) if h == head => Some(tail),
            _ => None,
        })
        .collect();
    format!("the offline bakes: {}", kids.join(", "))
}

/// Run whichever verb matched.
fn run(m: &clap::ArgMatches) -> Outcome {
    let path = |m: &clap::ArgMatches, id: &str| m.get_one::<String>(id).map(PathBuf::from);
    let text = |m: &clap::ArgMatches, id: &str| m.get_one::<String>(id).cloned();

    match m.subcommand() {
        Some(("new", a)) => {
            let dir = path(a, "DIR").expect("required");
            let stamp = text(a, "engine-version").unwrap_or_else(crate::distribution_version);
            let template =
                text(a, "template").unwrap_or_else(|| crate::templates::EMPTY.to_string());
            if !crate::templates::known(&template) {
                eprintln!(
                    "unknown template \"{template}\" — try one of: {}",
                    crate::templates::names().join(", ")
                );
                return Outcome::Exit(2);
            }
            Outcome::Exit(crate::new_project(&dir, &stamp, &template))
        }
        Some(("templates", _)) => {
            crate::print_templates();
            Outcome::Exit(0)
        }
        Some(("open", a)) => {
            Outcome::Launch { project: path(a, "PROJECT"), player: false, steam: false }
        }
        Some(("play", a)) => Outcome::Launch {
            project: path(a, "PROJECT"),
            player: true,
            steam: a.get_flag("steam"),
        },
        Some(("export", a)) => {
            let project = path(a, "PROJECT").expect("required");
            let out = path(a, "OUT").expect("required");
            let platform = text(a, "PLATFORM").expect("required");
            let title = text(a, "title").unwrap_or_else(|| default_title(&project));
            Outcome::Exit(crate::export::headless_export(&project, &out, &platform, &title))
        }
        Some(("migrate", a)) => {
            let dir = path(a, "DIR").expect("required");
            let stamp = text(a, "engine-version").unwrap_or_else(crate::distribution_version);
            Outcome::Exit(crate::migrate_project(&dir, &stamp))
        }
        Some(("bake", a)) => match a.subcommand() {
            Some(("clips", b)) => {
                let project = path(b, "PROJECT").expect("required");
                let model = text(b, "MODEL").expect("required");
                Outcome::Exit(crate::extract_clips_cmd(&project, &model))
            }
            Some(("gi", b)) => Outcome::Exit(crate::bake::run(
                &path(b, "PROJECT").unwrap_or_else(|| PathBuf::from("assets")),
                text(b, "scene").as_deref(),
                b.get_flag("json"),
            )),
            Some(("nav", b)) => Outcome::Exit(crate::bake::run_nav(
                &path(b, "PROJECT").unwrap_or_else(|| PathBuf::from("assets")),
                text(b, "scene").as_deref(),
                b.get_flag("json"),
            )),
            _ => Outcome::Exit(2),
        },
        Some(("exec", a)) => {
            let script = path(a, "SCRIPT").expect("required");
            let project = path(a, "PROJECT").unwrap_or_else(|| PathBuf::from("assets"));
            Outcome::Exit(crate::exec::run(&project, &script, a.get_flag("json")))
        }
        Some(("shot", a)) => {
            let project = path(a, "PROJECT").unwrap_or_else(|| PathBuf::from("assets"));
            let scene = text(a, "scene");
            let size = match text(a, "size").as_deref().map(crate::shot::parse_size) {
                Some(Err(e)) => {
                    eprintln!("{e}");
                    return Outcome::Exit(2);
                }
                Some(Ok(s)) => s,
                // 960x540: a sixteen-by-nine frame big enough to read a label
                // in and small enough to render and look at in a second.
                None => (960, 540),
            };
            let out = path(a, "out")
                .unwrap_or_else(|| crate::shot::default_out(&project, scene.as_deref()));
            Outcome::Exit(crate::shot::run(
                &project,
                scene.as_deref(),
                text(a, "camera").as_deref(),
                size,
                &out,
                a.get_flag("json"),
                a.get_flag("timing"),
            ))
        }
        Some(("vfx", a)) => {
            let project = path(a, "PROJECT").unwrap_or_else(|| PathBuf::from("assets"));
            let effect = text(a, "effect").expect("required");
            let at = match text(a, "at").as_deref().map(crate::vfx_shot::parse_times) {
                Some(Err(e)) => {
                    eprintln!("{e}");
                    return Outcome::Exit(2);
                }
                Some(Ok(t)) => Some(t),
                None => None,
            };
            let frames = match text(a, "frames").as_deref().map(str::parse::<usize>) {
                Some(Ok(0)) => {
                    eprintln!("--frames 0 asks for no pictures");
                    return Outcome::Exit(2);
                }
                Some(Ok(n)) => n,
                Some(Err(_)) => {
                    eprintln!("--frames wants a whole number of moments");
                    return Outcome::Exit(2);
                }
                // Five: enough to tell a start from a middle from an end with
                // two spare, and few enough to look at in one sheet.
                None => 5,
            };
            let size = match text(a, "size").as_deref().map(crate::shot::parse_size) {
                Some(Err(e)) => {
                    eprintln!("{e}");
                    return Outcome::Exit(2);
                }
                Some(Ok(s)) => s,
                // Square, because an effect on the bare stage has no aspect of
                // its own — and small enough that five of them tile into a sheet
                // nothing has to shrink to show.
                None => (480, 480),
            };
            let background = match text(a, "background").as_deref().map(crate::vfx_shot::parse_color)
            {
                Some(Err(e)) => {
                    eprintln!("{e}");
                    return Outcome::Exit(2);
                }
                Some(Ok(c)) => Some(c),
                None => None,
            };
            Outcome::Exit(crate::vfx_shot::run(crate::vfx_shot::Args {
                root: &project,
                effect: &effect,
                scene: text(a, "scene").as_deref(),
                camera: text(a, "camera").as_deref(),
                at,
                frames,
                size,
                background,
                out: path(a, "out"),
                json: a.get_flag("json"),
            }))
        }
        Some(("run", a)) => {
            let project = path(a, "PROJECT").unwrap_or_else(|| PathBuf::from("assets"));
            // **Parsed here, and parsed as what it is.** Not by clap's value
            // parser, so a bad number reads as a usage error with the flag
            // named — and `--frames` as a whole number rather than a float,
            // because every float that is not one is a silent wrong answer:
            // `nan` passes any comparison you write and casts to 0, so the verb
            // ran nothing and exited 0; `inf` casts to `u32::MAX`, which is
            // eight hundred days of simulated time and reads as a hang; and
            // `2.7` quietly becomes 2. A frame count is an integer.
            let frames = match text(a, "frames") {
                None => None,
                Some(v) => match v.parse::<u32>() {
                    Ok(0) => {
                        eprintln!("--frames wants at least one frame");
                        return Outcome::Exit(2);
                    }
                    Ok(n) => Some(n),
                    Err(_) => {
                        eprintln!("--frames wants a whole number of frames, not {v:?}");
                        return Outcome::Exit(2);
                    }
                },
            };
            // Seconds really is a float — half a second is a fair thing to ask
            // for — so the check is that it names a length: finite, positive.
            let seconds = match text(a, "seconds") {
                None => None,
                Some(v) => match v.parse::<f32>() {
                    Ok(s) if s.is_finite() && s > 0.0 => Some(s),
                    Ok(_) => {
                        eprintln!("--seconds wants a positive length of time, not {v:?}");
                        return Outcome::Exit(2);
                    }
                    Err(_) => {
                        eprintln!("--seconds wants a number, not {v:?}");
                        return Outcome::Exit(2);
                    }
                },
            };
            if frames.is_some() && seconds.is_some() {
                eprintln!("--frames and --seconds both say how long to run; pick one");
                return Outcome::Exit(2);
            }
            let span = match (frames, seconds) {
                (_, Some(s)) => crate::run::Span::Seconds(s),
                (Some(f), _) => crate::run::Span::Frames(f),
                // Two seconds of simulated time: long enough for a `start` to
                // run, a body to fall and a script to raise, short enough to
                // sit through after every edit.
                (None, None) => crate::run::Span::Frames(120),
            };
            let seed = match text(a, "seed") {
                None => None,
                Some(v) => match v.trim().parse::<u32>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        eprintln!("--seed wants a whole number from 0 to 4294967295, not {v:?}");
                        return Outcome::Exit(2);
                    }
                },
            };
            Outcome::Exit(crate::run::run(
                &project,
                text(a, "scene").as_deref(),
                span,
                crate::run::Options {
                    json: a.get_flag("json"),
                    steam: a.get_flag("steam"),
                    timing: a.get_flag("timing"),
                    alloc: a.get_flag("alloc"),
                    seed,
                },
            ))
        }
        Some(("serve", a)) => {
            // **Built for the runtime's own parser, not parsed again here.**
            // `ServerArgs::parse` is the one hand-written parser in this repo
            // with tests, it validates every value (a tick rate of zero, a port
            // that is not a number) and it reports unknown flags rather than
            // ignoring them. Handing it the shape it expects means `serve` and
            // `--server` cannot disagree about what a flag means.
            let project = path(a, "PROJECT").unwrap_or_else(|| PathBuf::from("assets"));
            let mut argv: Vec<String> =
                vec!["--server".into(), project.to_string_lossy().into_owned()];
            for flag in ["scene", "port", "relay", "tick", "interest", "budget"] {
                if let Some(v) = text(a, flag) {
                    argv.push(format!("--{flag}"));
                    argv.push(v);
                }
            }
            // **Somewhere to listen is a command-line question, so it is asked
            // here.** The server answers it with the same code it uses for "this
            // scene cannot be served", and those are 2 and 1 in this command
            // line — a caller has to be able to tell "you typed it wrong" from
            // "your project is wrong". Asking first is what makes every 2 the
            // server returns afterwards unambiguous.
            if text(a, "port").is_none() && text(a, "relay").is_none() {
                eprintln!(
                    "a dedicated server needs somewhere to listen: --port <n> or --relay <addr>"
                );
                return Outcome::Exit(2);
            }
            match floptle_runtime::server::ServerArgs::parse(&argv) {
                Ok(args) => Outcome::Exit(match floptle_runtime::server::run(args) {
                    // The server's "cannot serve this scene" is this command
                    // line's 1: the project is wrong, not the invocation. Its 3
                    // (could not bind, relay refused) passes through as its own
                    // thing, because neither of those is either.
                    2 => 1,
                    other => other,
                }),
                Err(e) => {
                    eprintln!("{e}");
                    Outcome::Exit(2)
                }
            }
        }
        Some(("doctor", a)) => Outcome::Exit(crate::doctor::run(a.get_flag("json"))),
        Some(("api", a)) => Outcome::Exit(crate::ide::cli_reference(
            text(a, "QUERY").as_deref(),
            a.get_flag("json"),
        )),
        Some(("inspect", a)) => {
            let project = path(a, "PROJECT").unwrap_or_else(|| PathBuf::from("assets"));
            Outcome::Exit(crate::inspect::run(
                &project,
                text(a, "scene").as_deref(),
                text(a, "select").as_deref(),
                a.get_flag("json"),
            ))
        }
        Some(("lint", a)) => {
            let project = path(a, "PROJECT").unwrap_or_else(|| PathBuf::from("assets"));
            // `--vec3` is accepted and is currently the only lint there is, so
            // its absence means the same thing rather than nothing. When a
            // second one arrives this becomes a choice; until then, refusing to
            // run without a flag whose only value is the default would be
            // ceremony.
            Outcome::Exit(crate::lint_vec3::run(&project, a.get_flag("json")))
        }
        Some(("check", a)) => {
            let project = path(a, "PROJECT").unwrap_or_else(|| PathBuf::from("assets"));
            Outcome::Exit(crate::check::run(&project, a.get_flag("json")))
        }
        Some(("version", a)) => {
            if a.get_flag("json") {
                println!(
                    "{}",
                    serde_json::json!({
                        "engine": floptle_core::ENGINE_NAME,
                        "version": crate::distribution_version(),
                    })
                );
            } else {
                println!("{} {}", floptle_core::ENGINE_NAME, crate::distribution_version());
            }
            Outcome::Exit(0)
        }
        Some(("help", a)) => {
            let verb = text(a, "VERB");
            if a.get_flag("json") {
                return help_json(verb.as_deref());
            }
            help_text(verb.as_deref())
        }
        _ => Outcome::Legacy,
    }
}

/// The title an export takes when none was given: the project directory's name.
fn default_title(project: &Path) -> String {
    project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "game".into())
}

/// `floptle help [VERB]`.
fn help_text(verb: Option<&str>) -> Outcome {
    let mut cmd = command();
    let Some(name) = verb else {
        let _ = cmd.print_long_help();
        println!();
        return Outcome::Exit(0);
    };
    // A nested verb can be asked for either way: `help bake` or `help "bake gi"`.
    let mut parts = name.split_whitespace();
    // `floptle help " "` is a verb name made only of spaces. It used to exit 2
    // saying nothing at all, which is the one thing a help command must never
    // do.
    let Some(head) = parts.next() else {
        eprintln!("`{name}` is not a verb name — try `floptle help`");
        return Outcome::Exit(1);
    };
    let Some(mut sub) = cmd.find_subcommand_mut(head).cloned() else {
        eprintln!("no such verb: {name} — try `floptle help`");
        return Outcome::Exit(1);
    };
    for p in parts {
        let Some(next) = sub.find_subcommand_mut(p).cloned() else {
            eprintln!("no such verb: {name} — try `floptle help`");
            return Outcome::Exit(1);
        };
        sub = next;
    }
    let _ = sub.print_long_help();
    println!();
    Outcome::Exit(0)
}

/// One verb, as the JSON `help --json` publishes.
fn verb_json(v: &Verb) -> serde_json::Value {
    let args: Vec<serde_json::Value> = v
        .args
        .iter()
        .map(|a| {
            let mut o = serde_json::json!({
                "name": a.name,
                "kind": if a.is_option() { "option" } else { "positional" },
                "type": a.value.type_name(),
                "required": a.required,
                "help": a.help,
            });
            if let Some(c) = a.value.choices() {
                o["choices"] = serde_json::json!(c);
            }
            o
        })
        .collect();
    let mut exits = serde_json::Map::new();
    for (code, why) in COMMON_EXITS.iter().chain(v.exits.iter()) {
        exits.insert(code.to_string(), serde_json::json!(why));
    }
    serde_json::json!({
        "name": v.name,
        "summary": v.summary,
        "detail": v.detail,
        "needsGpu": v.needs_gpu,
        "writesProject": v.writes_project,
        "args": args,
        "exitCodes": exits,
        "output": v.output,
        "replaces": v.legacy,
    })
}

/// `floptle help --json` — the whole surface, for a caller that cannot read
/// help text.
fn help_json(verb: Option<&str>) -> Outcome {
    if let Some(name) = verb {
        let Some(v) = VERBS.iter().find(|v| v.name == name) else {
            eprintln!("no such verb: {name} — try `floptle help --json`");
            return Outcome::Exit(1);
        };
        println!("{}", serde_json::to_string_pretty(&verb_json(v)).unwrap_or_default());
        return Outcome::Exit(0);
    }
    let doc = serde_json::json!({
        "engine": floptle_core::ENGINE_NAME,
        "version": crate::distribution_version(),
        "verbs": VERBS.iter().map(verb_json).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    Outcome::Exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("floptle".to_string()).chain(rest.iter().map(|s| s.to_string())).collect()
    }

    /// **The parser the table builds has to be a valid one.** `clap` panics on
    /// a malformed command at build time rather than at parse time, so a row
    /// with a duplicate id or an impossible argument would otherwise surface as
    /// a crash on somebody's first `--help`.
    #[test]
    fn the_table_builds_a_parser() {
        command().debug_assert();
    }

    /// Every verb has to say what it returns and what its own failures mean.
    /// An empty `output` or a verb that can fail without documenting code 1 is
    /// a row somebody stopped writing halfway, and `help --json` would publish
    /// the gap as though it were an answer.
    #[test]
    fn every_row_is_complete() {
        for v in VERBS {
            assert!(!v.summary.is_empty(), "{} has no summary", v.name);
            assert!(!v.output.is_empty(), "{} does not say what it returns", v.name);
            for (code, why) in v.exits {
                assert!(!why.is_empty(), "{} documents exit {code} with nothing", v.name);
                assert!(
                    !COMMON_EXITS.iter().any(|(c, _)| c == code),
                    "{} redefines the shared exit code {code}",
                    v.name
                );
            }
            for a in v.args {
                assert!(!a.help.is_empty(), "{} ▸ {} has no help", v.name, a.name);
                assert!(
                    a.is_option() || a.name.chars().all(|c| c.is_ascii_uppercase() || c == '_'),
                    "{} ▸ {} is a positional, so it is named in caps",
                    v.name,
                    a.name
                );
            }
        }
    }

    /// **Every old flag names the verb that replaced it.** The flags are a
    /// shipped interface and they keep working; what this guards is that a
    /// caller reading `help --json` can find out where each one went, which is
    /// the whole deprecation story and is worth nothing if a flag is missing
    /// from the table.
    #[test]
    fn every_shipped_flag_is_accounted_for() {
        const SHIPPED: &[&str] = &[
            "--new",
            "--template",
            "--list-templates",
            "--play",
            "--export",
            "--migrate",
            "--extract-clips",
            "--bake-gi",
            "--engine-version",
            "--version",
            "-V",
            "--help",
            "-h",
        ];
        for flag in SHIPPED {
            let named = VERBS.iter().any(|v| v.legacy.contains(flag))
                || VERBS.iter().any(|v| v.args.iter().any(|a| a.name == *flag));
            assert!(named, "{flag} still ships and no verb says what replaced it");
        }
    }

    /// **A command line the new parser does not own falls through untouched.**
    /// This is the whole compatibility promise: the Hub, CI and anybody's
    /// scripts reach the loop that has always served them.
    #[test]
    fn anything_that_is_not_a_verb_is_left_alone() {
        for line in [
            vec![],
            vec!["--version"],
            vec!["--new", "somewhere"],
            vec!["--export", "proj", "out", "host"],
            vec!["assets"],
            vec!["--play", "assets"],
            vec!["--template", "flappy", "--new", "x"],
        ] {
            assert!(
                matches!(dispatch(&argv(&line)), Outcome::Legacy),
                "{line:?} was claimed by the new parser and must not be"
            );
        }
    }

    /// …and a verb IS claimed, including a nested one.
    #[test]
    fn a_verb_is_claimed() {
        for line in [vec!["version"], vec!["help"], vec!["bake"], vec!["open"]] {
            assert!(
                !matches!(dispatch(&argv(&line)), Outcome::Legacy),
                "{line:?} is a verb and was not claimed"
            );
        }
    }

    /// The window verbs report what they want the editor started as, rather
    /// than starting it themselves — there is one launch path and this is not
    /// a second one.
    #[test]
    fn the_window_verbs_ask_for_a_launch() {
        assert!(matches!(dispatch(&argv(&["open", "assets"])), Outcome::Launch { player: false, .. }));
        assert!(matches!(
            dispatch(&argv(&["play"])),
            Outcome::Launch { project: None, player: true, steam: false }
        ));
        // `--steam` is how a dev tries Steam on a project with no App ID yet;
        // it must reach the launch, or `floptle play --steam` silently plays
        // without Steam — which is exactly what it was typed to prevent.
        assert!(matches!(
            dispatch(&argv(&["play", "--steam"])),
            Outcome::Launch { project: None, player: true, steam: true }
        ));
        // …and `bake gi` is NOT one of them any more. It renders offscreen and
        // exits, which is what its help always claimed.
        assert!(!matches!(dispatch(&argv(&["bake", "gi", "no-such-project"])), Outcome::Launch { .. }));
    }

    /// A usage mistake exits 2, which is the code CI depends on and the one
    /// nothing wrote down before.
    #[test]
    fn a_usage_mistake_exits_two() {
        assert!(matches!(dispatch(&argv(&["export", "only-one-argument"])), Outcome::Exit(2)));
        assert!(matches!(dispatch(&argv(&["bake"])), Outcome::Exit(2)));
        assert!(matches!(dispatch(&argv(&["new"])), Outcome::Exit(2)));
    }

    /// **`help --json` is an interface**, so the shape callers read is held
    /// here rather than left to whatever the last edit produced.
    #[test]
    fn the_published_surface_has_the_shape_callers_read() {
        let v = VERBS.iter().find(|v| v.name == "export").expect("export");
        let j = verb_json(v);
        assert_eq!(j["name"], "export");
        assert_eq!(j["needsGpu"], false, "export runs with no window and no GPU");
        assert_eq!(j["exitCodes"]["0"], "the operation succeeded");
        assert_eq!(
            j["exitCodes"]["2"],
            "the command line was wrong, including a PROJECT that is not a project directory"
        );
        assert!(j["exitCodes"]["1"].is_string(), "export can fail and must say so");
        assert_eq!(j["replaces"][0], "--export");
        let plat = j["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "PLATFORM")
            .expect("PLATFORM");
        let choices = plat["choices"].as_array().expect("the platforms are a fixed set");
        assert!(choices.iter().any(|c| c == "host"));
        // …and they come from the distribution's own list, so a new platform
        // cannot be published by the exporter and missing from the help.
        for p in floptle_dist::PLATFORMS {
            assert!(choices.iter().any(|c| c == p), "{p} exports and is not offered");
        }
        assert!(choices.iter().any(|c| c == floptle_dist::WEB_PLATFORM), "the browser exports and is not offered");
    }

    /// **A verb that exists is a verb the design page lists.**
    ///
    /// `docs/export-builds.md` is what somebody picking this up reads, and its
    /// verb block marks with `[x]` what is built. A verb added to the table and
    /// not to that list is a surface nobody is told about; a tick against a verb
    /// that does not exist is worse, because it is a promise. Same class as the
    /// package-binding docs test — the docs and the live surface are diffed
    /// rather than trusted to move together.
    #[test]
    fn every_verb_is_listed_on_the_design_page() {
        let page = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/export-builds.md");
        let text = std::fs::read_to_string(&page)
            .unwrap_or_else(|e| panic!("read {}: {e}", page.display()));
        // The block that lists the verb set, ticked for what is built.
        let ticked: Vec<String> = text
            .lines()
            .filter_map(|l| l.strip_prefix("[x] floptle "))
            .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
            .collect();
        for v in VERBS {
            // A nested verb is listed once as `bake gi | clips`, so match on the
            // group; a flat one matches on its own name.
            let head = v.split().0;
            assert!(
                ticked.iter().any(|t| t == v.name || t.starts_with(head)),
                "`floptle {}` is built and docs/export-builds.md does not tick it",
                v.name
            );
        }
        for t in &ticked {
            let head = t.split(' ').next().unwrap_or(t);
            assert!(
                VERBS.iter().any(|v| v.name == *t || v.split().0 == head),
                "docs/export-builds.md ticks `floptle {t}` and no such verb exists"
            );
        }
    }

    /// Every verb reaches `help <verb>`, including the nested ones under their
    /// full name.
    #[test]
    fn every_verb_can_be_asked_about() {
        for v in VERBS {
            assert!(
                matches!(help_text(Some(v.name)), Outcome::Exit(0)),
                "`floptle help {}` did not find it",
                v.name
            );
            assert!(matches!(help_json(Some(v.name)), Outcome::Exit(0)));
        }
        assert!(matches!(help_text(Some("nonsense")), Outcome::Exit(1)));
        assert!(matches!(help_json(Some("nonsense")), Outcome::Exit(1)));
    }
}
