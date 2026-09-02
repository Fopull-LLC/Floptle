#!/usr/bin/env bash
#
# The ADR-0028 bench gate on a REAL GAME: frame p95 under one script VM against
# another, on projects nobody wrote for a benchmark.
#
#   scripts/scene-bench.sh "$HOME/Floptle Projects/Forgery" first
#   scripts/scene-bench.sh solar system
#   scripts/scene-bench.sh solar system 900 5      # steps, repeats
#
# Which VMs to compare comes from $VMS (default "luajit luau"); the FIRST is the
# baseline every other is reported against. Any name `scripts/vm.sh` accepts
# works, so the code-generator lever is a third column rather than a second run:
#
#   VMS="luau luau-codegen" scripts/scene-bench.sh solar system
#
# ## Why this exists next to `examples/vm_bench.rs`
#
# `vm_bench` is a synthetic scene: 400 nodes running one shape of Lua each, so
# that a regression can be attributed to a shape. It is deliberately not a game.
# The plan's gate is the other half of that — Solar's system scene and a
# Forgery-shaped first-person scene, i.e. scripts written to make something
# work rather than to be measured. A probe that only ever measured its own
# workload can report a pass that a real project does not get.
#
# ## Why the whole editor gets built once per VM
#
# `vm-luajit` and `vm-luau` are mutually exclusive Cargo features and two Luas
# cannot link into one process, so there is no in-process comparison to make.
# Each VM gets its own release binary, copied aside, and the runs are
# INTERLEAVED (a, b, a, b…) rather than run in blocks — a machine that warms up,
# throttles, or picks up a background job partway through would otherwise hand
# the whole difference to whichever VM ran last.
#
# ## What the number is
#
# `floptle run --timing --json`, whose `timing.p95_ms` is the wall-clock cost of
# one headless step: world streaming plus the whole play step (scripts, the
# fixed tick, physics), with no render and no present. Steps the session was
# HELD for — the Play-start terrain hold, which steps with `dt = 0` — are
# excluded by `--timing` itself and reported separately; they are a loading
# screen, they are cheap, and how many of them there are varies per run, so
# letting them in would compare terrain workers rather than VMs.
#
# Reported per repeat and as the MEDIAN of the repeats, because one run of
# anything is an anecdote.
#
# ## What it is not
#
# It is not a CI assertion, and must not become one. A perf guard in this repo
# asserts a ratio measured within one process (see the CI notes in HANDOFF);
# this compares two processes built from two feature sets, which is exactly the
# comparison that burned v0.34.0's release gate. It is a probe a human reads.
#
# ## And bench a COPY of a project you care about
#
# `floptle run` opens a project the way the editor does, which tops up its
# seeded files. That is harmless and it is still a write, so point this at a
# copy of anything that is not under version control.

set -euo pipefail

cd "$(dirname "$0")/.."

PROJECT="${1:-}"
SCENE="${2:-}"
STEPS="${3:-900}"
REPEATS="${4:-5}"

if [ -z "$PROJECT" ]; then
  echo "usage: scripts/scene-bench.sh <project-dir> [scene] [steps] [repeats]" >&2
  echo "   eg: scripts/scene-bench.sh solar system 900 5" >&2
  exit 2
fi
if [ ! -f "$PROJECT/project.ron" ]; then
  echo "scene-bench: $PROJECT is not a project directory (no project.ron)" >&2
  exit 2
fi

OUT="${SCENE_BENCH_OUT:-$PWD/.bench}"
mkdir -p "$OUT"

# Which VMs to compare. The FIRST is the baseline; every other is reported as a
# ratio against it.
read -r -a VM_LIST <<<"${VMS:-luajit luau}"
if [ "${#VM_LIST[@]}" -lt 2 ]; then
  echo "scene-bench: \$VMS needs at least two VMs to compare (got '${VMS:-}')" >&2
  exit 2
fi

# **Built, then copied aside.** Every feature set writes the same
# `target/release/floptle`, so each build replaces the last — comparing them
# means keeping a copy of each before the next build runs.
#
# Always through `vm.sh`, including for whichever VM is currently the workspace
# default: `--no-default-features --features vm-<name>` says what it means no
# matter which one that is, so this script does not quietly start measuring the
# default against itself the day the default moves.
build() {  # build <vm>
  local vm="$1"
  echo "scene-bench: building $vm (release)…" >&2
  scripts/vm.sh "$vm" build -p floptle-editor --release --bin floptle >&2
  cp "${CARGO_TARGET_DIR:-$HOME/.cache/floptle-target}/release/floptle" "$OUT/floptle-$vm"
}

for vm in "${VM_LIST[@]}"; do build "$vm"; done

# `--scene` is optional: with none, the project's own entry scene runs, which is
# the right default for a project whose entry IS the thing to measure.
scene_args=()
if [ -n "$SCENE" ]; then scene_args=(--scene "$SCENE"); fi

# One run. Prints the p95 in milliseconds on stdout and nothing else, so the
# caller can collect it; everything the project itself said goes to the log file
# named on stderr, because a project that raises during a bench is a fact about
# the bench.
one() {  # one <vm> <repeat>
  local vm="$1" i="$2"
  local log="$OUT/$vm-$i.json"
  # `run` exits 1 when the project raises, and a real game's first seconds
  # legitimately warn. The exit code is reported, not obeyed — a run that
  # ERRORED is still a run whose steps were timed, and hiding it would be worse
  # than saying so.
  set +e
  "$OUT/floptle-$vm" run "$PROJECT" "${scene_args[@]}" \
    --frames "$STEPS" --timing --json >"$log" 2>"$OUT/$vm-$i.err"
  local code=$?
  set -e
  python3 scripts/scene_bench_report.py "$log" "$vm" "$i" "$code"
}

echo >&2
echo "scene-bench: $PROJECT${SCENE:+ / $SCENE} — $STEPS steps x $REPEATS repeats, interleaved across: ${VM_LIST[*]}" >&2
echo >&2

# Collected as "<vm> <p95>" lines rather than one array per VM, so the number of
# VMs is data instead of code.
samples=""
for i in $(seq 1 "$REPEATS"); do
  for vm in "${VM_LIST[@]}"; do
    samples+="$vm $(one "$vm" "$i")"$'\n'
  done
done

echo >&2
printf '%s' "$samples" | python3 scripts/scene_bench_summary.py "${VM_LIST[0]}"
