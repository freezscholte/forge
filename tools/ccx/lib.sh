# ccx thin harness: shared rebuild mechanics for run-task.sh / verify-task.sh.
# Sourced, not executed. Stack commits go on a detached HEAD so the base ref
# NEVER moves (pilot amendment P1, transcribed from
# experiments/ccx/run-arm-a-stacked.sh).

# ccx_check_clone <clone> <base>
# Pre-flight: the base ref must resolve to a commit in the clone and the
# clone must carry full history (stacked rebuilds on shallow clones fail in
# confusing ways). Prints an actionable message and returns nonzero on
# failure.
ccx_check_clone() {
  local clone="$1" base="$2"
  # Refuse to operate on the harness's OWN repository. ccx_rebuild_base is
  # destructive (reset --hard + clean -fdq), so a --clone that points at the
  # forge checkout containing tools/ccx (the CLAUDE.md project-root gotcha)
  # or the operator's live working copy would erase uncommitted work. The
  # scratch clone must live elsewhere (e.g. under /tmp).
  local harness_root clone_root
  harness_root="$(git -C "${BASH_SOURCE[0]%/*}" rev-parse --show-toplevel 2>/dev/null || true)"
  clone_root="$(git -C "$clone" rev-parse --show-toplevel 2>/dev/null || true)"
  echo "ccx: operating on clone $clone_root (base $base)" >&2
  if [[ -n "$harness_root" && -n "$clone_root" && "$harness_root" == "$clone_root" ]]; then
    echo "ccx: clone pre-flight failed: --clone resolves to the harness's own repo ($clone_root)" >&2
    echo "ccx: rebuild is destructive — use a disposable scratch clone outside this repo (e.g. under /tmp)" >&2
    return 1
  fi
  if ! git -C "$clone" rev-parse --verify --quiet "${base}^{commit}" >/dev/null 2>&1; then
    echo "ccx: clone pre-flight failed: ref '$base' does not resolve in $clone" >&2
    echo "ccx: fetch it first (git -C '$clone' fetch origin '$base') or pass a ref that exists in the clone" >&2
    return 1
  fi
  if [[ "$(git -C "$clone" rev-parse --is-shallow-repository 2>/dev/null)" != "false" ]]; then
    echo "ccx: clone pre-flight failed: $clone is a shallow repository" >&2
    echo "ccx: stacked rebuilds need full history — run: git -C '$clone' fetch --unshallow" >&2
    return 1
  fi
}

# ccx_abspath <path>
# Absolutize a FILE path against the invoking cwd. Needed before any path
# operand crosses a `git -C`/`cd` boundary (NER-384/NER-385 bug family).
# NOTE: callers must capture via `x="$(ccx_abspath p)" || fail` — the guard
# fires on the function's exit status. Never inline a second command
# substitution into the same assignment: an assignment's exit status is that
# of its LAST substitution, which silently masks an earlier failure.
ccx_abspath() {
  local dir
  dir="$(cd "$(dirname "$1")" && pwd)" || return 1
  printf '%s/%s\n' "$dir" "$(basename "$1")"
}

# ccx_rebuild_base <clone> <base-ref> [stack-patch...]
# Rebuild the exact task base: hard-reset to <base>, clean untracked state
# (keeping target/), detach HEAD at <base>, apply each stack patch with
# --index --3way in order, then record a single stack commit when any patch
# applied. Empty patch files (e.g. a dry-run predecessor's patch.diff) are
# no-ops. Returns nonzero with a message on any failure.
ccx_rebuild_base() {
  local clone="$1" base="$2"
  shift 2
  git -C "$clone" reset --hard --quiet "$base" || {
    echo "ccx: reset --hard to '$base' failed in $clone" >&2
    return 1
  }
  git -C "$clone" clean -fdq -e target || {
    echo "ccx: clean failed in $clone" >&2
    return 1
  }
  git -C "$clone" checkout --quiet --detach "$base" || {
    echo "ccx: detached checkout of '$base' failed in $clone" >&2
    return 1
  }
  local applied=0 patch abs_patch
  for patch in "$@"; do
    # A missing patch is an error, never a silent no-op: with `git -C` the
    # relative path would otherwise resolve INSIDE the clone (and can even
    # hit an unrelated same-path file committed there), so absolutize
    # against the invoking cwd first — the same dir the -s check reads.
    if [[ ! -e "$patch" ]]; then
      echo "ccx: stack patch not found: $patch" >&2
      return 1
    fi
    [[ -s "$patch" ]] || continue # empty patch is a no-op stack entry
    abs_patch="$(ccx_abspath "$patch")" || {
      echo "ccx: cannot resolve stack patch path: $patch" >&2
      return 1
    }
    if ! git -C "$clone" apply --index --3way "$abs_patch"; then
      echo "ccx: stack patch $patch failed to apply on '$base'" >&2
      return 1
    fi
    applied=$((applied + 1))
  done
  if ((applied > 0)); then
    git -C "$clone" -c user.name=ccx -c user.email=ccx@local \
      commit --quiet -m "ccx stack: $applied patch(es) on $base" || {
      echo "ccx: stack commit failed in $clone" >&2
      return 1
    }
  fi
}
