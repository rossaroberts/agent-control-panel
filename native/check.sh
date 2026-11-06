#!/bin/sh
# The whole native gate: format, lints, tests, then the parity against Node's machine.mjs, then the budgets.
# Run from anywhere: sh native/check.sh
set -e
cd "$(dirname "$0")"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release -p fleetd
node parity/collect-parity.mjs
# Task 10 adds the bench; until it exists the gate has nothing to run there.
if [ -f bench/run.mjs ]; then node bench/run.mjs; fi
