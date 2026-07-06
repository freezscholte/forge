#!/usr/bin/env bash
# ccx thin harness: independent acceptance re-verification (README.md
# §Exit codes). Rebuild the exact task base (stack), apply the task's own
# patch, run the contract's acceptance fix set then guard set fresh, and
# record set-labeled PASS/FAIL per command in <out>/verify.txt.
#
# Exit codes:
#   0  fix + guard all green
#   1  usage / rebuild failure (incl. PATCH-FAIL)
#   2  any fix failure (guards still run and are recorded)
#   4  fix all green but a guard regressed — "task works but broke
#      something pre-existing", mechanically distinguishable on purpose
set -uo pipefail
CCX="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$CCX/lib.sh"

usage() {
  cat >&2 <<'EOF'
usage: verify-task.sh --clone <dir> --base <ref> --contract <file>
                      --contracts-dir <dir> --out <dir>
                      [--stack <patch.diff>]... [--patch <patch.diff>]
--patch defaults to <out>/patch.diff
EOF
  exit 1
}

CLONE="" BASE="" CONTRACT="" CONTRACTS_DIR="" OUT="" PATCH=""
STACKS=()
while (($#)); do
  case "$1" in
    --clone) CLONE="${2:?}"; shift 2 ;;
    --base) BASE="${2:?}"; shift 2 ;;
    --contract) CONTRACT="${2:?}"; shift 2 ;;
    --contracts-dir) CONTRACTS_DIR="${2:?}"; shift 2 ;;
    --out) OUT="${2:?}"; shift 2 ;;
    --stack) STACKS+=("${2:?}"); shift 2 ;;
    --patch) PATCH="${2:?}"; shift 2 ;;
    *) echo "verify-task: unknown option: $1" >&2; usage ;;
  esac
done
[[ -n "$CLONE" && -n "$BASE" && -n "$CONTRACT" && -n "$CONTRACTS_DIR" && -n "$OUT" ]] || usage
[[ -n "$PATCH" ]] || PATCH="$OUT/patch.diff"
[[ -f "$PATCH" ]] || { echo "verify-task: patch not found: $PATCH" >&2; exit 1; }

mkdir -p "$OUT" || { echo "verify-task: cannot create out dir $OUT" >&2; exit 1; }
VERIFY="$OUT/verify.txt"
: > "$VERIFY"
echo "=== ccx verify :: $(basename "$CONTRACT") :: stack[${#STACKS[@]}] :: $(date +%H:%M:%S)"

# 1. Read the contract's acceptance sets up front (before touching the
# clone): a malformed contract is a usage failure, not a verify result.
# --dump-acceptance is fail-closed — it refuses (nonzero) any command that
# would not pass the rule-6 grammar/no-metacharacter check, so this eval
# sink is gated even when verify-task.sh is invoked standalone without the
# runner's lint preflight.
if ! ACC_JSON="$(python3 "$CCX/ccx-lint.py" --contracts-dir "$CONTRACTS_DIR" \
  --dump-acceptance "$CONTRACT")"; then
  echo "verify-task: cannot read acceptance from $CONTRACT (malformed, or an unsafe acceptance command was refused)" >&2
  exit 1
fi
SCHEMA="$(python3 -c 'import sys, yaml; print(yaml.safe_load(open(sys.argv[1])).get("schema", ""))' "$CONTRACT" 2>/dev/null)"
if [[ "$SCHEMA" == "ccx.contract.v0" ]]; then
  echo "WARN: v0 flat acceptance treated as fix set"
fi
FIX_CMDS=()
GUARD_CMDS=()
while IFS= read -r line; do FIX_CMDS+=("$line"); done < <(
  python3 -c 'import json, sys
for c in json.load(sys.stdin)["fix"]: print(c)' <<< "$ACC_JSON")
while IFS= read -r line; do GUARD_CMDS+=("$line"); done < <(
  python3 -c 'import json, sys
for c in json.load(sys.stdin)["guard"]: print(c)' <<< "$ACC_JSON")

# 2. Rebuild the exact stacked base on a detached HEAD (P1: base never moves).
if ! ccx_check_clone "$CLONE" "$BASE"; then
  echo "FATAL: clone pre-flight failed" >&2
  exit 1
fi
if ! ccx_rebuild_base "$CLONE" "$BASE" ${STACKS[@]+"${STACKS[@]}"}; then
  echo "FATAL: base rebuild failed" >&2
  exit 1
fi

# 3. Apply the task's own patch (empty patch is a no-op, as in lib.sh).
if [[ -s "$PATCH" ]] && ! git -C "$CLONE" apply --index --3way "$PATCH"; then
  echo "PATCH-FAIL" >> "$VERIFY"
  sed 's/^/    /' "$VERIFY"
  exit 1
fi

# 4. Run the fix set, then the guard set — guards always run even when a
# fix command failed, so the record is complete.
FIX_FAILED=0 GUARD_FAILED=0
run_set() { # <label> <cmd>...
  local label="$1" cmd rc
  shift
  for cmd in "$@"; do
    if (cd "$CLONE" && eval "$cmd" > /dev/null 2>&1); then
      echo "$label PASS $cmd" >> "$VERIFY"
    else
      echo "$label FAIL $cmd" >> "$VERIFY"
      rc=1
    fi
  done
  [[ -z "${rc:-}" ]]
}
run_set FIX ${FIX_CMDS[@]+"${FIX_CMDS[@]}"} || FIX_FAILED=1
run_set GUARD ${GUARD_CMDS[@]+"${GUARD_CMDS[@]}"} || GUARD_FAILED=1
sed 's/^/    /' "$VERIFY"

if ((FIX_FAILED)); then
  exit 2
elif ((GUARD_FAILED)); then
  exit 4
fi
exit 0
