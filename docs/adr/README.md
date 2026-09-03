# Architecture decision records

The engine's code cites a few of these by number — `// ADR-0028` beside the
thing the decision produced — so the two that shipped code refers to live here,
where following the citation lands somewhere.

An ADR is one decision: what the situation was, what was chosen, *why*, what was
rejected, and what the choice obligates afterwards. They are not updated when a
decision changes; a later one supersedes them, so the reasoning history stays
readable.

| # | Decision | Status |
|---|---|---|
| [0027](0027-a-command-line-an-agent-can-drive.md) | **A command line an agent can drive** — subcommands on the one binary, one verb table, JSON on request | Accepted |
| [0028](0028-one-script-vm-everywhere-and-a-webgpu-only-browser-target.md) | **One script VM everywhere (Luau)**, and a WebGPU-only browser target | Accepted |

The decisions these two build on are referenced by number in their headers.
