#!/usr/bin/env bash
# ccx pilot brief emitter (T1 spike) — deterministic, no LLM, byte-stable.
#
# Usage: brief.sh <contract-file.yaml>
# Emits: global policy + the task contract + its neighbor contracts
# (one level, in declared order). Output is a pure function of the input
# files: no timestamps, no environment data, stable ordering — identical
# inputs must produce identical bytes (prompt-cache-friendly, reproducible).
set -euo pipefail

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/contracts"
contract="${1:?usage: brief.sh <contract-file.yaml>}"
[[ -f "$contract" ]] || contract="$dir/$(basename "$contract")"
[[ -f "$contract" ]] || { echo "no such contract: $1" >&2; exit 1; }

emit() {
  printf -- '--- %s ---\n' "$1"
  cat "$2"
  printf '\n'
}

emit "GLOBAL POLICY (normative)" "$dir/_global-policy.yaml"
emit "TASK CONTRACT (normative)" "$contract"

# Neighbor contracts: ids listed under `neighbors:` as `- ccx-...`.
# Resolution: neighbor id `ccx-foo` -> contracts/foo.yaml. Declared order.
grep -E '^[[:space:]]*-[[:space:]]+ccx-' "$contract" | sed -E 's/^[[:space:]]*-[[:space:]]+(ccx-[a-z0-9-]+).*/\1/' |
while read -r nid; do
  nfile="$dir/${nid#ccx-}.yaml"
  if [[ -f "$nfile" ]]; then
    emit "NEIGHBOR CONTRACT (normative): $nid" "$nfile"
  else
    printf -- '--- NEIGHBOR CONTRACT MISSING: %s (surface as unknown, do not guess) ---\n\n' "$nid"
  fi
done
