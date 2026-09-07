//! `floptle-fleet` — the fleet agent (`floptle/0197` §6, `floptle/0199` §3).
//!
//! It is the only piece of the dedicated-server story that starts a process.
//! Everything else — the builds API, the deployment rows, the portal — is a
//! description of what should be running; this turns that into servers on a box
//! and reports back what actually happened.
//!
//! ```text
//!   GET /desired ─▶ ensure engine + bundle ─▶ reconcile units ─▶ POST /status
//!        ▲                                                            │
//!        └──────────────────── every --interval seconds ◀─────────────┘
//! ```
//!
//! Run it on the region's box, as one systemd unit, with a box token supplied
//! through `LoadCredential`. `docs/fleet-agent.md` is the operator's page.

mod agent;
mod args;
mod bundle;
mod engine;
mod fetch;
mod unit;
mod wire;

use args::Args;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match Args::parse_argv(&argv) {
        Ok(Some(a)) => a,
        // `--help` is a help request and exits 0. This is also how an operator
        // tells a current binary from one already on a box — the same check W
        // uses on the relay, after the July build read `--help` as a port.
        Ok(None) => {
            print!("{}", Args::HELP);
            return;
        }
        Err(e) => {
            eprintln!("floptle-fleet: {e}");
            std::process::exit(2);
        }
    };

    // **No token, no start.** The alternative is a box that polls an endpoint
    // which refuses it every ten seconds forever, looking alive in `systemctl`
    // and doing nothing — the untracked path the relay refuses for the same
    // reason.
    if args.token.is_none() && !args.dry_run {
        eprintln!(
            "floptle-fleet: no box token. Give --token-file, or run under systemd with \
             LoadCredential=fleet-token:<path>, or set FLOPTLE_FLEET_TOKEN.\n\
             Without one every poll would be refused and this box would look healthy \
             while running nothing."
        );
        std::process::exit(2);
    }

    bundle::log_line(&format!(
        "floptle-fleet {} — region {}, control {}, every {}s{}",
        env!("CARGO_PKG_VERSION"),
        args.region,
        args.control,
        args.interval,
        if args.dry_run { " (dry run)" } else { "" }
    ));

    let mut host = agent::RealHost;
    let mut state = agent::Agent::default();
    loop {
        match fetch::get_desired(&args) {
            Ok(desired) => {
                match state.cycle(&args, &mut host, &desired) {
                    Ok(report) => {
                        if args.dry_run {
                            bundle::log_line(&format!(
                                "would report: {}",
                                serde_json::to_string(&report.to_json()).unwrap_or_default()
                            ));
                        } else if let Err(e) = fetch::post_status(&args, &report) {
                            // Worth a line every time: a status that is not
                            // landing means a stopped deployment's port is not
                            // being released, which nothing else will notice.
                            bundle::log_line(&format!("status not reported: {e}"));
                        }
                    }
                    Err(e) => bundle::log_line(&format!("cycle failed: {e}")),
                }
            }
            // **A failed poll changes nothing on the box.** Treating "I could
            // not ask" as "nothing should be running" would take a whole region
            // down on a bad minute at the website.
            Err(e) => bundle::log_line(&format!("{e} — leaving the box as it is")),
        }
        if args.once {
            return;
        }
        agent::nap(args.interval);
    }
}
