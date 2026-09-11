#!/usr/bin/env bash
#
# Run a cargo command against a chosen script VM (ADR-0028).
#
#   scripts/vm.sh luau         test -p floptle-editor --test docs_index
#   scripts/vm.sh luau-codegen clippy --all-targets    # every VM-carrying crate
#
# Why this exists: a VM feature is selected by turning defaults OFF and naming
# the feature again, on every crate that carries it, and Cargo features are
# additive. Luau is the only VM since v0.89.0 (the `vm-luajit` escape hatch was
# removed on ADR-0028's schedule); what the script still switches is the code
# generator (`luau-codegen`), which is a benchmarking lever.
#
# Get it wrong and the error you see is mlua-sys's ("You can enable only one of
# the features: lua54, lua53, …"), which names none of the features you wrote
# and says the same thing for none as for two.
#
# Two things this deliberately does NOT do:
#
#   * `--workspace --no-default-features` — that would strip defaults with
#     nothing to do with the VM (the audio backend, the gamepad backend). The
#     VM-carrying packages are run one at a time instead.
#   * hard-code which crates those are — a crate carries the switch if its
#     manifest declares `vm-luau`, the same rule `tests/vm_wiring.rs` uses. A
#     new forwarder is picked up on its own; a stale list would not be.

set -euo pipefail

cd "$(dirname "$0")/.."

VM="${1:-}"
shift || true

case "$VM" in
  # `luau-codegen` works because the feature is literally named
  # `vm-luau-codegen` — the same `vm-$VM` substitution reaches it, and it
  # implies `vm-luau` in every carrier. It is a benchmarking lever, not a
  # third VM.
  luau|luau-codegen) ;;
  luajit)
    echo "scripts/vm.sh: the vm-luajit escape hatch was removed in v0.89.0 (ADR-0028);" >&2
    echo "  Luau is the only VM the engine embeds. Use: scripts/vm.sh luau <cargo args...>" >&2
    exit 2
    ;;
  *)
    echo "usage: scripts/vm.sh <luau|luau-codegen> <cargo args...>" >&2
    echo "   eg: scripts/vm.sh luau-codegen test -p floptle-script" >&2
    exit 2
    ;;
esac

if [ "$#" -eq 0 ]; then
  echo "scripts/vm.sh: no cargo command given (try: test -p floptle-script)" >&2
  exit 2
fi

carriers=()
for m in crates/*/Cargo.toml; do
  if grep -q '^vm-luau = \[' "$m"; then
    carriers+=("$(basename "$(dirname "$m")")")
  fi
done

if [ "${#carriers[@]}" -eq 0 ]; then
  echo "scripts/vm.sh: no crate declares a vm-luau feature — has the switch been removed?" >&2
  exit 1
fi

# The feature flags must land on CARGO's side of a `--`, or they are handed to
# the test binary and come back as "Unrecognized option: 'no-default-features'".
# So the argument list is split at the first bare `--` and the flags are spliced
# into the left half.
pre=(); post=(); seen_sep=0
for a in "$@"; do
  if [ "$seen_sep" -eq 0 ] && [ "$a" = "--" ]; then
    seen_sep=1
    continue
  fi
  if [ "$seen_sep" -eq 0 ]; then pre+=("$a"); else post+=("$a"); fi
done

# `--features vm-luau` is a BARE feature name, which cargo resolves against the
# selected package. That is why one package runs at a time: `pkg/feature` syntax
# would need every named package to be in the selection, and `-p floptle-script`
# is not.
run() {  # run <extra cargo args...>
  local sel=("$@")
  if [ "$seen_sep" -eq 1 ]; then
    cargo "${pre[@]}" "${sel[@]}" --no-default-features --features "vm-$VM" -- "${post[@]}"
  else
    cargo "${pre[@]}" "${sel[@]}" --no-default-features --features "vm-$VM"
  fi
}

if printf '%s\n' "${pre[@]}" | grep -qx -- '-p\|--package'; then
  echo "scripts/vm.sh: $VM" >&2
  run
  exit $?
fi

# No package named: run the command once per VM-carrying crate.
status=0
for c in "${carriers[@]}"; do
  echo "scripts/vm.sh: $VM  -p $c" >&2
  run -p "$c" || status=$?
done
exit "$status"
