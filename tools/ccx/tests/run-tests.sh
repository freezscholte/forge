#!/usr/bin/env bash
# CCX harness self-test entrypoint: python suites (stdlib unittest only — no
# pytest dependency) plus the shell tests. Fails when zero python tests were
# collected: a pytest-style suite under unittest discovery silently collects
# nothing and goes green vacuously — the exact Goodhart case lint rule 4
# exists to catch, so this gate refuses to reproduce it.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

python3 -c 'import yaml' 2>/dev/null || {
  echo "run-tests: PyYAML missing — python3 -m pip install pyyaml" >&2
  exit 1
}

echo "==> ccx python suites (unittest discover)"
out="$(python3 -m unittest discover -s tools/ccx/tests -p 'test_*.py' 2>&1)"
status=$?
echo "$out" | tail -3
[[ $status -eq 0 ]] || exit 1
count="$(echo "$out" | grep -Eo 'Ran [0-9]+ tests?' | grep -Eo '[0-9]+' | head -1)"
if [[ "${count:-0}" -eq 0 ]]; then
  echo "run-tests: zero python tests collected — vacuous green refused" >&2
  exit 1
fi

# Propagate shell-suite failures: without -e and with each suite as a
# non-final command, a failing suite would otherwise be swallowed and the
# gate would go green — the exact vacuous-pass hazard this entrypoint exists
# to prevent.
echo "==> ccx shell suite: test_runner.sh"
bash tools/ccx/tests/test_runner.sh || exit 1

echo "==> ccx shell suite: test_verify.sh"
bash tools/ccx/tests/test_verify.sh || exit 1

echo
echo "ccx harness self-tests passed ($count python tests + 2 shell suites)."
