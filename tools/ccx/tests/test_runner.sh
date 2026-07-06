#!/usr/bin/env bash
# ccx thin harness: shell tests for run-task.sh + lib.sh. Standalone:
#   bash tools/ccx/tests/test_runner.sh
# Builds a scratch git repo per run; every scenario uses a fresh clone and
# the --dry-run or stub agents (no real agent sessions). Exits nonzero on
# any failure and prints PASS/FAIL per scenario.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
CCX="$ROOT/tools/ccx"
FIX="$CCX/tests/fixtures"
RUN="$CCX/run-task.sh"
CHAIN_A="$FIX/runner-chain-a.yaml"
CHAIN_B="$FIX/runner-chain-b.yaml"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAILURES=0
pass() { echo "PASS: $1"; }
fail() {
  echo "FAIL: $1"
  FAILURES=$((FAILURES + 1))
  if [[ -f "${2:-}" ]]; then
    sed 's/^/      | /' "$2"
  fi
}

# --- scratch repo with a `base` branch --------------------------------------
SCRATCH="$TMP/scratch"
git init -q "$SCRATCH"
git -C "$SCRATCH" config user.name ccx-test
git -C "$SCRATCH" config user.email ccx@test
mkdir -p "$SCRATCH/src" "$SCRATCH/notes"
echo "hello" > "$SCRATCH/src/main.txt"
echo "notes" > "$SCRATCH/notes/README.txt"
git -C "$SCRATCH" add -A
git -C "$SCRATCH" commit -qm "init scratch"
git -C "$SCRATCH" branch base
BASE_SHA="$(git -C "$SCRATCH" rev-parse base)"

mkclone() { # <dir>
  git clone -q "$SCRATCH" "$1"
  git -C "$1" branch -q base origin/base
  git -C "$1" config user.name ccx-test
  git -C "$1" config user.email ccx@test
}

# --- stack patches: stack2 only applies after stack1 ------------------------
PGEN="$TMP/patchgen"
mkclone "$PGEN"
echo "one" > "$PGEN/src/order.txt"
git -C "$PGEN" add -A
git -C "$PGEN" diff --cached > "$TMP/stack1.diff"
git -C "$PGEN" commit -qm "s1"
printf 'one\ntwo\n' > "$PGEN/src/order.txt"
git -C "$PGEN" add -A
git -C "$PGEN" diff --cached > "$TMP/stack2.diff"

# --- stub agents -------------------------------------------------------------
cat > "$TMP/agent-unknown.sh" <<'EOF'
#!/usr/bin/env bash
cat > /dev/null
printf 'kind: blocking\nquestion: test fixture unknown\n' > UNKNOWN.md
EOF
cat > "$TMP/agent-evil.sh" <<'EOF'
#!/usr/bin/env bash
cat > /dev/null
echo "rogue write outside allowed_changes" > outside.txt
EOF
# Crashed/unauthenticated agent: nonzero exit, no UNKNOWN.md, no patch.
cat > "$TMP/agent-crash.sh" <<'EOF'
#!/usr/bin/env bash
cat > /dev/null
exit 7
EOF
chmod +x "$TMP/agent-unknown.sh" "$TMP/agent-evil.sh" "$TMP/agent-crash.sh"

echo "this is not a unified diff" > "$TMP/garbage.diff"

# --- 1. detached-head invariant (P1 pin) -------------------------------------
C="$TMP/c1"; O="$TMP/o1"; LOG="$TMP/log1"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --stack "$TMP/stack1.diff" --dry-run "$CHAIN_A" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 0 \
  && "$(git -C "$C" rev-parse base)" == "$BASE_SHA" ]] \
  && ! git -C "$C" symbolic-ref -q HEAD > /dev/null \
  && [[ -f "$C/.ccx-dry-run-marker" ]]; then
  pass "1 detached-head invariant: base ref pinned, HEAD detached, agent ran"
else
  fail "1 detached-head invariant (rc=$rc base=$(git -C "$C" rev-parse base))" "$LOG"
fi

# --- 2. stack order: both patches applied, in order ---------------------------
C="$TMP/c2"; O="$TMP/o2"; LOG="$TMP/log2"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --stack "$TMP/stack1.diff" --stack "$TMP/stack2.diff" --dry-run "$CHAIN_A" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 0 && "$(cat "$C/src/order.txt" 2>/dev/null)" == "$(printf 'one\ntwo')" ]]; then
  pass "2 stack order: both stack patches applied in order"
else
  fail "2 stack order (rc=$rc order.txt=$(cat "$C/src/order.txt" 2>/dev/null))" "$LOG"
fi

# --- 3. halt-on-unknown halts the chain ---------------------------------------
C="$TMP/c3"; O="$TMP/o3"; LOG="$TMP/log3"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --agent-cmd "$TMP/agent-unknown.sh" --chain "$CHAIN_A" "$CHAIN_B" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 2 && -f "$O/runner-chain-a/UNKNOWN.md" \
  && ! -e "$O/runner-chain-b/result.json" ]]; then
  pass "3 halt-on-unknown: exit 2, UNKNOWN.md preserved, dependent task not run"
else
  fail "3 halt-on-unknown (rc=$rc)" "$LOG"
fi

# --- 4. lint gate fires before any agent cost ---------------------------------
C="$TMP/c4"; O="$TMP/o4"; LOG="$TMP/log4"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --dry-run "$FIX/lint-bad-grammar.yaml" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 1 && ! -e "$C/.ccx-dry-run-marker" ]]; then
  pass "4 lint gate: contract with lint ERROR exits 1 before the agent runs"
else
  fail "4 lint gate (rc=$rc)" "$LOG"
fi

# --- 5. failing stack patch is fatal before any agent cost --------------------
C="$TMP/c5"; O="$TMP/o5"; LOG="$TMP/log5"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --stack "$TMP/garbage.diff" --dry-run "$CHAIN_A" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 1 && ! -e "$C/.ccx-dry-run-marker" ]]; then
  pass "5 failing stack patch: exit 1, no agent run"
else
  fail "5 failing stack patch (rc=$rc)" "$LOG"
fi

# --- 6. blast postflight catches out-of-radius writes -------------------------
C="$TMP/c6"; O="$TMP/o6"; LOG="$TMP/log6"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --agent-cmd "$TMP/agent-evil.sh" "$CHAIN_A" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 3 && -s "$O/runner-chain-a/blast.json" \
  && -s "$O/runner-chain-a/patch.diff" ]]; then
  pass "6 blast integration: out-of-radius write exits 3, artifacts preserved"
else
  fail "6 blast integration (rc=$rc)" "$LOG"
fi

# --- 7. chain refusal: dependency neither in chain nor stacked ----------------
C="$TMP/c7"; O="$TMP/o7"; LOG="$TMP/log7"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --dry-run --chain "$CHAIN_B" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 1 && ! -e "$C/.ccx-dry-run-marker" \
  && ! -e "$O/runner-chain-b/brief.txt" ]]; then
  pass "7 chain refusal: missing dependency refused before any run"
else
  fail "7 chain refusal (rc=$rc)" "$LOG"
fi

# --- 8. prompt composition: brief + task-instruction, byte-exact --------------
if cmp -s <(cat "$TMP/o1/runner-chain-a/brief.txt" "$CCX/prompts/task-instruction.txt") \
  "$TMP/o1/runner-chain-a/prompt.txt"; then
  pass "8 prompt composition: prompt.txt == brief.txt + task-instruction.txt bytes"
else
  fail "8 prompt composition: byte mismatch" "$TMP/log1"
fi

# --- 9. crashed agent (nonzero exit, no UNKNOWN) is fatal ---------------------
C="$TMP/c9"; O="$TMP/o9"; LOG="$TMP/log9"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --agent-cmd "$TMP/agent-crash.sh" "$CHAIN_A" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 1 ]] && grep -q "agent exited nonzero" "$LOG"; then
  pass "9 crashed agent: nonzero exit with no UNKNOWN is fatal (not silent success)"
else
  fail "9 crashed agent (rc=$rc)" "$LOG"
fi

# --- 10. --stack count guard: one patch cannot cover two missing deps ---------
cat > "$TMP/runner-multidep.yaml" <<'EOF'
schema: ccx.contract.v1
id: ccx-runner-multidep
revision: 1
ticket: NER-000
task: Two out-of-chain dependencies
interface: |
  Depends on two contracts that are not in the chain.
depends_on: [ccx-ext-one, ccx-ext-two]
acceptance:
  fix: [cargo build]
  guard: []
allowed_changes:
  paths: [src/**]
authority: {source: human, confidence: high, reviewer: ccx-harness-tests}
EOF
C="$TMP/c10"; O="$TMP/o10"; LOG="$TMP/log10"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$O" \
  --stack "$TMP/stack1.diff" --dry-run --chain "$TMP/runner-multidep.yaml" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 1 ]] && grep -q "out-of-chain dependencies but only 1" "$LOG"; then
  pass "10 stack count guard: 2 missing deps + 1 --stack is refused"
else
  fail "10 stack count guard (rc=$rc)" "$LOG"
fi

# --- 11. relative --out: agent redirections resolve against invoking cwd ------
# Regression (2026-07-06 NER-362 dogfood): the agent step cd's into the clone
# before redirecting to "$out/...", so a relative --out used to fatal with
# "prompt.txt: No such file or directory" once the real agent step ran.
C="$TMP/c11"; LOG="$TMP/log11"
mkclone "$C"
mkdir -p "$TMP/w11"
(cd "$TMP/w11" && "$RUN" --clone "$C" --base base --contracts-dir "$FIX" \
  --out rel-out --dry-run "$CHAIN_A") > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 0 && -f "$TMP/w11/rel-out/runner-chain-a/prompt.txt" \
  && -f "$TMP/w11/rel-out/runner-chain-a/patch.diff" \
  && ! -e "$C/rel-out" ]]; then
  pass "11 relative --out: canonicalized to invoking cwd, nothing lands in clone"
else
  fail "11 relative --out (rc=$rc)" "$LOG"
fi

# --- 12. relative --stack: patch path resolves against invoking cwd -----------
# Regression (2026-07-06 NER-362 dogfood): git -C resolves a relative patch
# path inside the clone; a patch that exists only in the invoking cwd failed
# with "can't open patch" — and a same-path file committed INSIDE the clone
# would silently be used instead. Also: a missing patch must be fatal, not a
# silent no-op skip.
C="$TMP/c12"; LOG="$TMP/log12"
mkclone "$C"
mkdir -p "$TMP/w12"
cp "$TMP/stack1.diff" "$TMP/w12/rel-stack.diff"
(cd "$TMP/w12" && "$RUN" --clone "$C" --base base --contracts-dir "$FIX" \
  --out out12 --stack rel-stack.diff --dry-run "$CHAIN_A") > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 0 && "$(cat "$C/src/order.txt" 2>/dev/null)" == "one" ]]; then
  pass "12a relative --stack: patch resolved against invoking cwd and applied"
else
  fail "12a relative --stack (rc=$rc)" "$LOG"
fi
C="$TMP/c12b"; LOG="$TMP/log12b"
mkclone "$C"
"$RUN" --clone "$C" --base base --contracts-dir "$FIX" --out "$TMP/o12b" \
  --stack "$TMP/does-not-exist.diff" --dry-run "$CHAIN_A" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 1 ]] && grep -q "stack patch not found" "$LOG" \
  && [[ ! -e "$C/.ccx-dry-run-marker" ]]; then
  pass "12b missing --stack patch: fatal before any agent run, not a silent skip"
else
  fail "12b missing --stack patch (rc=$rc)" "$LOG"
fi

echo
if ((FAILURES > 0)); then
  echo "test_runner: $FAILURES scenario(s) FAILED"
  exit 1
fi
echo "test_runner: all scenarios passed"
