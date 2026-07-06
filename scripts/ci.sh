#!/usr/bin/env bash
# Run the full CI suite locally — the identical checks .github/workflows/ci.yml
# runs on every push/PR, in the same order. Run this before pushing to catch
# failures without burning a CI round-trip.
#
# Usage:  bash scripts/ci.sh
# Exit:   0 if everything passes, non-zero (and stops) at the first failure.
#
# Note: the e2e step's DB-inspection checks need `sqlite3` on PATH (preinstalled
# on macOS; `apt-get install sqlite3` on Linux). Without it those checks SKIP
# rather than fail — CI installs it so they always run there.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

echo "==> fmt    (cargo fmt --all --check)"
cargo fmt --all --check

echo "==> test   (cargo test --workspace)"
cargo test --workspace

echo "==> clippy (cargo clippy --workspace --all-targets -- -D warnings)"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> lines  (scripts/check-rust-line-count.sh)"
bash scripts/check-rust-line-count.sh

echo "==> ccx    (tools/ccx/tests/run-tests.sh)"
bash tools/ccx/tests/run-tests.sh

echo "==> e2e    (scripts/e2e-eval.sh)"
bash scripts/e2e-eval.sh

echo
echo "All CI checks passed locally."
