#!/usr/bin/env bash
# ccx thin harness: dependency-ordered runner (README.md §Lifecycle).
# Per task: lint preflight → stacked detached-HEAD base rebuild → brief +
# task-instruction prompt → fresh agent session → collect patch.diff →
# halt-on-unknown → blast postflight.
#
# Exit codes (README.md §Exit codes):
#   0  all tasks ran within blast radius
#   1  usage / fatal (lint error, brief failure, stack apply failure, ...)
#   2  UNKNOWN filed — chain halted for author triage (a success outcome)
#   3  blast violation — artifacts preserved, chain halted
set -uo pipefail
CCX="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$CCX/lib.sh"

usage() {
  cat >&2 <<'EOF'
usage: run-task.sh --clone <dir> --base <ref> --contracts-dir <dir> --out <dir>
                   [--stack <patch.diff>]... [--agent-cmd <cmd>] [--dry-run]
                   (<contract.yaml> | --chain <contract.yaml>...)
EOF
  exit 1
}

CLONE="" BASE="" CONTRACTS_DIR="" OUT=""
AGENT_CMD="claude -p --output-format json --dangerously-skip-permissions"
DRY_RUN=0 CHAIN=0
STACKS=()
CONTRACTS=()
while (($#)); do
  case "$1" in
    --clone) CLONE="${2:?}"; shift 2 ;;
    --base) BASE="${2:?}"; shift 2 ;;
    --contracts-dir) CONTRACTS_DIR="${2:?}"; shift 2 ;;
    --out) OUT="${2:?}"; shift 2 ;;
    --stack) STACKS+=("${2:?}"); shift 2 ;;
    --agent-cmd) AGENT_CMD="${2:?}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --chain) CHAIN=1; shift ;;
    -*) echo "run-task: unknown option: $1" >&2; usage ;;
    *) CONTRACTS+=("$1"); shift ;;
  esac
done
[[ -n "$CLONE" && -n "$BASE" && -n "$CONTRACTS_DIR" && -n "$OUT" ]] || usage
((${#CONTRACTS[@]} >= 1)) || usage
if ((!CHAIN)) && ((${#CONTRACTS[@]} > 1)); then
  echo "run-task: multiple contracts require --chain" >&2
  usage
fi
if ((DRY_RUN)); then
  # No-op agent: proves the agent step ran without spending a session.
  AGENT_CMD="touch .ccx-dry-run-marker"
fi

# run_one <contract> <task-name> [stack-patch...]
run_one() {
  local contract="$1" task="$2"
  shift 2
  local out="$OUT/$task"
  mkdir -p "$out" || { echo "FATAL: cannot create out dir $out"; exit 1; }
  # Clear a stale UNKNOWN.md from a prior invocation of this same task, so a
  # clean re-run after triage is not misread as still-blocked.
  rm -f "$out/UNKNOWN.md"
  echo "=== ccx run :: $task :: stack[$#] :: $(date +%H:%M:%S)"

  # 1. Rebuild the exact stacked base on a detached HEAD (P1: base never moves).
  if ! ccx_check_clone "$CLONE" "$BASE"; then
    echo "FATAL: clone pre-flight failed for $task"
    exit 1
  fi
  if ! ccx_rebuild_base "$CLONE" "$BASE" "$@"; then
    echo "FATAL: base rebuild failed for $task"
    exit 1
  fi

  # 2. Lint preflight against the REBUILT clone — a contract that fails lint
  # never reaches an agent. --repo-root pins primitive/cap resolution to the
  # clone (the tree the agent will see), not the caller's cwd; without it a
  # run launched from a /tmp scratch dir would falsely FATAL on every contract.
  if ! python3 "$CCX/ccx-lint.py" --contracts-dir "$CONTRACTS_DIR" \
       --repo-root "$CLONE" "$contract"; then
    echo "FATAL: lint gate failed for $contract — fix the contract before any agent run"
    exit 1
  fi

  # 3. Brief + single-sourced task instruction → prompt.
  if ! python3 "$CCX/ccx-brief.py" --contracts-dir "$CONTRACTS_DIR" "$contract" > "$out/brief.txt"; then
    echo "FATAL: brief emission failed for $contract"
    exit 1
  fi
  if ! cat "$out/brief.txt" "$CCX/prompts/task-instruction.txt" > "$out/prompt.txt"; then
    echo "FATAL: prompt composition failed for $task"
    exit 1
  fi

  # 4. Fresh agent session in the clone.
  local start end status
  start=$(date +%s)
  (cd "$CLONE" && bash -c "$AGENT_CMD" < "$out/prompt.txt" > "$out/result.json" 2> "$out/stderr.log")
  status=$?
  end=$(date +%s)
  echo "$status $((end - start))s" > "$out/exit-and-seconds.txt"

  # 5. Collect the produced patch. UNKNOWN.md and the dry-run marker are
  # harness control artifacts, not task output — keep both out of patch.diff
  # so a preserved patch never carries a stale UNKNOWN.md into a later stack.
  if ! git -C "$CLONE" add -A; then
    echo "FATAL: git add failed while collecting patch for $task"
    exit 1
  fi
  git -C "$CLONE" reset --quiet -- .ccx-dry-run-marker UNKNOWN.md 2>/dev/null
  git -C "$CLONE" diff --cached > "$out/patch.diff"

  # 6. Stop-on-unknown: a well-formed stop is a success outcome (see
  # UNKNOWN-TRIAGE.md) but the chain halts for author triage.
  if [[ -f "$CLONE/UNKNOWN.md" ]]; then
    cp "$CLONE/UNKNOWN.md" "$out/UNKNOWN.md"
    echo "HALT: $task filed UNKNOWN — chain stops here for author triage"
    exit 2
  fi

  # 6b. A crashed/unauthenticated agent (no UNKNOWN.md, nonzero exit) must not
  # pass as success — an empty patch would otherwise clear blast and let a
  # dependent stack an empty base (the silent clean-base run P1 forbids).
  if ((status != 0)); then
    echo "FATAL: agent exited nonzero ($status) for $task with no UNKNOWN.md — see $out/stderr.log"
    exit 1
  fi

  # 7. Blast postflight: the patch must stay inside the contract's radius.
  python3 "$CCX/ccx-blast.py" --diff --contract "$contract" \
    < "$out/patch.diff" > "$out/blast.json"
  local brc=$?
  if ((brc == 2)); then
    echo "VIOLATION: $task patch escapes the contract blast radius — artifacts preserved in $out"
    exit 3
  elif ((brc != 0)); then
    echo "FATAL: blast check errored (exit $brc) for $task"
    exit 1
  fi
  echo "    exit=$status wall=$((end - start))s patch=$(wc -l < "$out/patch.diff") lines"
}

if ((CHAIN)); then
  # Topologically order the chain by depends_on; refuse to start when a
  # dependency is neither among the chain contracts nor covered by an
  # explicit --stack patch. Prints one line per task, in run order:
  #   <contract-path> <TAB> <id> <TAB> <comma-sep in-chain dep ids, topo order>
  if ! PLAN="$(python3 - "${#STACKS[@]}" "${CONTRACTS[@]}" <<'EOF'
import sys

import yaml

stack_count = int(sys.argv[1])
paths = sys.argv[2:]
info = {}  # id -> (path, deps)
order_in = []
for p in paths:
    doc = yaml.safe_load(open(p))
    if not isinstance(doc, dict) or "id" not in doc:
        sys.stderr.write(f"run-task: {p}: not a contract mapping with an id\n")
        sys.exit(1)
    cid = doc["id"]
    if cid in info:
        sys.stderr.write(f"run-task: duplicate contract id in chain: {cid}\n")
        sys.exit(1)
    info[cid] = (p, list(doc.get("depends_on") or []))
    order_in.append(cid)

ids = set(info)
missing = sorted({d for cid in ids for d in info[cid][1] if d not in ids})
# Every out-of-chain dependency must be covered by its own --stack patch.
# Requiring stack_count >= len(missing) (not merely "any stack supplied")
# kills the reproduced bypass where one unrelated patch suppressed refusal
# for every missing dependency at once. NOTE: --stack patches are opaque
# files, so this is a count guard, not an id match; an operator can still
# supply the wrong N patches. Explicit per-id acknowledgement is a tracked
# follow-up (see the plan Open Questions).
if missing and len(missing) > stack_count:
    sys.stderr.write(
        "run-task: chain refusal: "
        + str(len(missing))
        + " out-of-chain dependencies but only "
        + str(stack_count)
        + " --stack patch(es): "
        + ", ".join(missing)
        + "\nrun-task: add the missing contracts to --chain, or supply one "
        "--stack patch per missing dependency\n"
    )
    sys.exit(1)

# Kahn topological order, stable on the given contract order.
placed, topo = set(), []
pending = list(order_in)
while pending:
    rest = []
    for cid in pending:
        deps = [d for d in info[cid][1] if d in ids]
        if all(d in placed for d in deps):
            placed.add(cid)
            topo.append(cid)
        else:
            rest.append(cid)
    if len(rest) == len(pending):
        sys.stderr.write(
            "run-task: depends_on cycle among chain contracts: "
            + ", ".join(rest) + "\n"
        )
        sys.exit(1)
    pending = rest

pos = {cid: i for i, cid in enumerate(topo)}


def closure(cid, acc):
    for d in info[cid][1]:
        if d in ids and d not in acc:
            acc.add(d)
            closure(d, acc)
    return acc


for cid in topo:
    deps = sorted(closure(cid, set()), key=pos.get)
    print(f"{info[cid][0]}\t{cid}\t{','.join(deps)}")
EOF
  )"; then
    exit 1
  fi
  while IFS=$'\t' read -r cpath cid cdeps; do
    [[ -n "$cpath" ]] || continue
    task="${cid#ccx-}"
    TSTACKS=(${STACKS[@]+"${STACKS[@]}"})
    if [[ -n "$cdeps" ]]; then
      IFS=',' read -ra DEPIDS <<< "$cdeps"
      for dep in "${DEPIDS[@]}"; do
        TSTACKS+=("$OUT/${dep#ccx-}/patch.diff")
      done
    fi
    run_one "$cpath" "$task" ${TSTACKS[@]+"${TSTACKS[@]}"}
  done <<< "$PLAN"
else
  contract="${CONTRACTS[0]}"
  if ! cid="$(python3 -c 'import sys, yaml; print(yaml.safe_load(open(sys.argv[1]))["id"])' "$contract")"; then
    echo "FATAL: cannot read contract id from $contract"
    exit 1
  fi
  run_one "$contract" "${cid#ccx-}" ${STACKS[@]+"${STACKS[@]}"}
fi
echo "ccx run COMPLETE $(date +%H:%M:%S)"
