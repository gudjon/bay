#!/bin/sh
# G171: Git spawns and Git time per bay command, for the Git on PATH, per object and ref format.
# Builds bay and the harness, then runs examples/git_cost.rs; arguments go to it, for example
#   scripts/git-cost.sh --commits 1000 --files 100000 --formats sha256/reftable --json
set -eu
cd "$(dirname "$0")/.."
target=${CARGO_TARGET_DIR:-target}
cargo build --quiet --bin bay --example git_cost
exec "$target/debug/examples/git_cost" --bay "$target/debug/bay" "$@"
