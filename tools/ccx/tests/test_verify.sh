#!/usr/bin/env bash
# ccx thin harness: shell tests for verify-task.sh. Standalone:
#   bash tools/ccx/tests/test_verify.sh
# Builds a scratch git repo per run; contracts are written on the fly into
# a temp contracts dir (verify-task.sh does not lint, so plain shell
# commands stand in for acceptance entries). Exits nonzero on any failure
# and prints PASS/FAIL per scenario.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
CCX="$ROOT/tools/ccx"
VERIFY="$CCX/verify-task.sh"

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
mkdir -p "$SCRATCH/src"
echo "hello" > "$SCRATCH/src/main.txt"
git -C "$SCRATCH" add -A
git -C "$SCRATCH" commit -qm "init scratch"
git -C "$SCRATCH" branch base

mkclone() { # <dir>
  git clone -q "$SCRATCH" "$1"
  git -C "$1" branch -q base origin/base
  git -C "$1" config user.name ccx-test
  git -C "$1" config user.email ccx@test
}

# --- patches -----------------------------------------------------------------
# patch-good: creates src/f.txt on the clean base.
PGEN="$TMP/patchgen"
mkclone "$PGEN"
echo "made it" > "$PGEN/src/f.txt"
git -C "$PGEN" add -A
git -C "$PGEN" diff --cached > "$TMP/patch-good.diff"

# stack1 adds src/order.txt; dependent modifies the line stack1 added, so
# it only applies on top of stack1 (mirrors the pilot clean-base
# impossibility).
SGEN="$TMP/stackgen"
mkclone "$SGEN"
echo "one" > "$SGEN/src/order.txt"
git -C "$SGEN" add -A
git -C "$SGEN" diff --cached > "$TMP/stack1.diff"
git -C "$SGEN" commit -qm "s1"
printf 'one\ntwo\n' > "$SGEN/src/order.txt"
git -C "$SGEN" add -A
git -C "$SGEN" diff --cached > "$TMP/dependent.diff"

# --- on-the-fly contracts -----------------------------------------------------
CONTRACTS="$TMP/contracts"
mkdir -p "$CONTRACTS"

cat > "$CONTRACTS/all-green.yaml" <<'EOF'
schema: ccx.contract.v1
id: ccx-all-green
acceptance:
  fix:
    - test -f src/f.txt
  guard:
    - "true"
EOF

cat > "$CONTRACTS/fix-fails.yaml" <<'EOF'
schema: ccx.contract.v1
id: ccx-fix-fails
acceptance:
  fix:
    - "false"
  guard:
    - "true"
EOF

cat > "$CONTRACTS/guard-fails.yaml" <<'EOF'
schema: ccx.contract.v1
id: ccx-guard-fails
acceptance:
  fix:
    - test -f src/f.txt
  guard:
    - "false"
EOF

cat > "$CONTRACTS/stacked.yaml" <<'EOF'
schema: ccx.contract.v1
id: ccx-stacked
acceptance:
  fix:
    - test -f src/order.txt
  guard: []
EOF

cat > "$CONTRACTS/v0-flat.yaml" <<'EOF'
schema: ccx.contract.v0
id: ccx-v0-flat
acceptance:
  - test -f src/f.txt
EOF

# --- 1. all green: exit 0, FIX PASS + GUARD PASS recorded ---------------------
C="$TMP/c1"; O="$TMP/o1"; LOG="$TMP/log1"
mkclone "$C"; mkdir -p "$O"
bash "$VERIFY" --clone "$C" --base base --contract "$CONTRACTS/all-green.yaml" \
  --contracts-dir "$CONTRACTS" --out "$O" --patch "$TMP/patch-good.diff" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 0 ]] \
  && grep -q "^FIX PASS test -f src/f.txt$" "$O/verify.txt" \
  && grep -q "^GUARD PASS true$" "$O/verify.txt"; then
  pass "1 all green: exit 0, FIX PASS and GUARD PASS recorded"
else
  fail "1 all green (rc=$rc)" "$LOG"
fi

# --- 2. fix failure: exit 2, guards still recorded ----------------------------
C="$TMP/c2"; O="$TMP/o2"; LOG="$TMP/log2"
mkclone "$C"; mkdir -p "$O"
bash "$VERIFY" --clone "$C" --base base --contract "$CONTRACTS/fix-fails.yaml" \
  --contracts-dir "$CONTRACTS" --out "$O" --patch "$TMP/patch-good.diff" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 2 ]] \
  && grep -q "^FIX FAIL false$" "$O/verify.txt" \
  && grep -q "^GUARD PASS true$" "$O/verify.txt"; then
  pass "2 fix failure: exit 2, guard set still ran and was recorded"
else
  fail "2 fix failure (rc=$rc)" "$LOG"
fi

# --- 3. guard-only failure: exit 4, fix green (the must-not-regress signal) ---
C="$TMP/c3"; O="$TMP/o3"; LOG="$TMP/log3"
mkclone "$C"; mkdir -p "$O"
bash "$VERIFY" --clone "$C" --base base --contract "$CONTRACTS/guard-fails.yaml" \
  --contracts-dir "$CONTRACTS" --out "$O" --patch "$TMP/patch-good.diff" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 4 ]] \
  && grep -q "^FIX PASS test -f src/f.txt$" "$O/verify.txt" \
  && grep -q "^GUARD FAIL false$" "$O/verify.txt"; then
  pass "3 guard-only failure: exit 4, FIX PASS + GUARD FAIL recorded"
else
  fail "3 guard-only failure (rc=$rc)" "$LOG"
fi

# --- 4a. stack rebuild: dependent patch verifies WITH --stack ------------------
C="$TMP/c4a"; O="$TMP/o4a"; LOG="$TMP/log4a"
mkclone "$C"; mkdir -p "$O"
bash "$VERIFY" --clone "$C" --base base --contract "$CONTRACTS/stacked.yaml" \
  --contracts-dir "$CONTRACTS" --out "$O" \
  --stack "$TMP/stack1.diff" --patch "$TMP/dependent.diff" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 0 ]] \
  && grep -q "^FIX PASS test -f src/order.txt$" "$O/verify.txt" \
  && [[ "$(cat "$C/src/order.txt" 2>/dev/null)" == "$(printf 'one\ntwo')" ]]; then
  pass "4a stack rebuild: dependent patch verifies with --stack"
else
  fail "4a stack rebuild with stack (rc=$rc)" "$LOG"
fi

# --- 4b. same patch WITHOUT the stack: exit 1, PATCH-FAIL recorded ------------
C="$TMP/c4b"; O="$TMP/o4b"; LOG="$TMP/log4b"
mkclone "$C"; mkdir -p "$O"
bash "$VERIFY" --clone "$C" --base base --contract "$CONTRACTS/stacked.yaml" \
  --contracts-dir "$CONTRACTS" --out "$O" \
  --patch "$TMP/dependent.diff" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 1 ]] && grep -q "^PATCH-FAIL$" "$O/verify.txt"; then
  pass "4b clean-base impossibility: exit 1, PATCH-FAIL recorded without --stack"
else
  fail "4b clean-base impossibility (rc=$rc)" "$LOG"
fi

# --- 5. v0 flat acceptance: runs as fix set, WARN emitted ---------------------
C="$TMP/c5"; O="$TMP/o5"; LOG="$TMP/log5"
mkclone "$C"; mkdir -p "$O"
bash "$VERIFY" --clone "$C" --base base --contract "$CONTRACTS/v0-flat.yaml" \
  --contracts-dir "$CONTRACTS" --out "$O" --patch "$TMP/patch-good.diff" > "$LOG" 2>&1
rc=$?
if [[ $rc -eq 0 ]] \
  && grep -q "^WARN: v0 flat acceptance treated as fix set$" "$LOG" \
  && grep -q "^FIX PASS test -f src/f.txt$" "$O/verify.txt" \
  && ! grep -q "^GUARD" "$O/verify.txt"; then
  pass "5 v0 flat acceptance: WARN emitted, flat list ran as fix set"
else
  fail "5 v0 flat acceptance (rc=$rc)" "$LOG"
fi

echo
if ((FAILURES)); then
  echo "test_verify: $FAILURES scenario(s) FAILED"
  exit 1
fi
echo "test_verify: all scenarios passed"
