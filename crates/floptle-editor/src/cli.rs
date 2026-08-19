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
    (2, "the command line was wrong"),
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
        args: &[Arg {
            name: "PROJECT",
            value: Value::Path,
            required: false,
            help: "the project directory (default: assets/)",
        }],
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
        summary: "bake a scene's light probes",
        detail: "**This opens the editor.** Unlike the other bakes it is not headless yet: it \
                 asks the editor to bake on load and then runs normally. Said plainly here \
                 because the flag it replaces claimed otherwise.",
        args: &[Arg {
            name: "PROJECT",
            value: Value::Path,
            required: false,
            help: "the project directory (default: assets/)",
        }],
        needs_gpu: true,
        writes_project: true,
        exits: &[],
        output: "the bake's progress in the editor's Console",
        legacy: &["--bake-gi"],
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
    Launch { project: Option<PathBuf>, player: bool, bake_gi: bool },
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
fn is_verb_head(arg: &str) -> bool {
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
    #[cfg(unix)]
    // SAFETY: setting a signal disposition to SIG_DFL before any thread has been
    // spawned, which is the disposition the process would have had if Rust had
    // not changed it at startup.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
    run(&m)
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
            Outcome::Launch { project: path(a, "PROJECT"), player: false, bake_gi: false }
        }
        Some(("play", a)) => {
            Outcome::Launch { project: path(a, "PROJECT"), player: true, bake_gi: false }
        }
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
            Some(("gi", b)) => {
                Outcome::Launch { project: path(b, "PROJECT"), player: false, bake_gi: true }
            }
            _ => Outcome::Exit(2),
        },
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
    let Some(head) = parts.next() else { return Outcome::Exit(2) };
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
        assert!(matches!(
            dispatch(&argv(&["open", "assets"])),
            Outcome::Launch { player: false, bake_gi: false, .. }
        ));
        assert!(matches!(
            dispatch(&argv(&["play"])),
            Outcome::Launch { project: None, player: true, bake_gi: false }
        ));
        assert!(matches!(
            dispatch(&argv(&["bake", "gi"])),
            Outcome::Launch { player: false, bake_gi: true, .. }
        ));
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
        assert_eq!(j["exitCodes"]["2"], "the command line was wrong");
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
    }

    /// **A verb that exists is a verb the design page lists.**
    ///
    /// `docs/cli-proposal.md` is what somebody picking this up reads, and its
    /// verb block marks with `[x]` what is built. A verb added to the table and
    /// not to that list is a surface nobody is told about; a tick against a verb
    /// that does not exist is worse, because it is a promise. Same class as the
    /// package-binding docs test — the docs and the live surface are diffed
    /// rather than trusted to move together.
    #[test]
    fn every_verb_is_listed_on_the_design_page() {
        let page = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/cli-proposal.md");
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
                "`floptle {}` is built and docs/cli-proposal.md does not tick it",
                v.name
            );
        }
        for t in &ticked {
            let head = t.split(' ').next().unwrap_or(t);
            assert!(
                VERBS.iter().any(|v| v.name == *t || v.split().0 == head),
                "docs/cli-proposal.md ticks `floptle {t}` and no such verb exists"
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
