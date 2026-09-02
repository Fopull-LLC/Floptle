"""One line per `scripts/scene-bench.sh` run: the distribution, on stderr.

Its own file rather than a heredoc inside the shell script, because the shell
script also runs `floptle run` inside a `<<'PY'` heredoc and a nested one ends
the outer at its first `PY` line — a corruption that shows up as a shell syntax
error a hundred lines from the mistake.

**stdout carries exactly one number, the p95**, so the caller can collect it in
a variable; everything a human reads goes to stderr.
"""

import json
import sys


def main() -> int:
    path, vm, i, code = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    try:
        with open(path) as f:
            doc = json.load(f)
    except (OSError, ValueError) as e:
        print(f"scene-bench: {vm} run {i} produced no JSON ({e})", file=sys.stderr)
        return 1

    timing = doc.get("timing")
    if not timing:
        print(
            f"scene-bench: {vm} run {i} reported no timing — is --timing on this build?",
            file=sys.stderr,
        )
        return 1

    # A run that raised is still a run whose steps were timed. Reported, not
    # hidden and not obeyed: a real game's first seconds legitimately warn, and
    # silently dropping the sample would be the worse of the two failures.
    note = "" if code == 0 else f"  [exit {code}: {doc.get('errors', 0)} error(s)]"
    # `samples` is the SIMULATING steps. `paused` is the Play-start hold, which
    # `--timing` excludes from the distribution; printing it stops a run that
    # spent half its span held from reading like one that did not.
    held = f", {timing['paused']} held" if timing.get("paused") else ""
    print(
        f"  {vm:<13} run {i}: p50 {timing['p50_ms']:6.2f}  p95 {timing['p95_ms']:6.2f}  "
        f"p99 {timing['p99_ms']:6.2f}  max {timing['max_ms']:7.2f}  "
        f"({timing['samples']} steps{held}, {doc.get('seconds', 0):.1f}s simulated){note}",
        file=sys.stderr,
    )
    print(timing["p95_ms"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
