"""The table `scripts/scene-bench.sh` ends with.

Reads `<vm> <p95>` lines on stdin — one per run, in the order they ran — and
prints each VM's median p95 plus its ratio against the baseline named in argv[1].

**The median, not the mean**, for the same reason the probe reports p95 rather
than an average frame: one run that happened to collide with something else on
the machine should not move the answer.

**The ratio is stated in the direction of the question.** The gate asks "is this
VM worse than the baseline", so the output says FASTER or SLOWER in words rather
than leaving a reader to work out which way a bare number points.
"""

import statistics
import sys


def main() -> int:
    baseline = sys.argv[1]
    by: dict[str, list[float]] = {}
    for line in sys.stdin.read().splitlines():
        if not line.strip():
            continue
        vm, val = line.rsplit(" ", 1)
        by.setdefault(vm, []).append(float(val))

    if baseline not in by:
        print(f"scene-bench: no samples for the baseline {baseline!r}", file=sys.stderr)
        return 1

    med = {vm: statistics.median(v) for vm, v in by.items()}
    width = max(len(v) for v in med)
    n = len(by[baseline])
    for vm, m in med.items():
        print(f"  {vm:<{width}} p95 (median of {n}): {m:.2f} ms")

    base = med[baseline]
    for vm, m in med.items():
        if vm == baseline:
            continue
        if m <= base:
            print(f"  -> {vm} is {base / m:.2f}x FASTER than {baseline} on this scene")
        else:
            print(f"  -> {vm} is {m / base:.2f}x SLOWER than {baseline} on this scene")
    return 0


if __name__ == "__main__":
    sys.exit(main())
