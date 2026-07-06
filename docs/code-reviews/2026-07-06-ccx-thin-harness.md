# Code review: CCX thin harness

- **Branch:** `experiment/ccx-spikes`
- **Plan:** `docs/plans/2026-07-06-001-feat-ccx-thin-harness-plan.md`
- **base-sha:** `f21022b` (pre-harness)
- **head-sha at review:** `e60d3dd` (U1–U7 landed)
- **head-sha after fixes:** `064e405` (`fix(review):`)
- **Reviewers:** correctness, security, adversarial (in-process), adversarial-codex (cross-model), testing, maintainability, project-standards, reliability, agent-native, learnings-researcher.
- **Context:** the harness exists to be a trustworthy gate; the review is the gate-layering lesson applied to the harness itself — real P1s in the gate logic surfaced *after* 36 self-tests + the full CI mirror were green. Multiple findings were reproduced by executing the scripts; the top three P1s were independently confirmed by the cross-model (Codex) pass at confidence 100.

## Real / actionable — FIXED (commit 064e405)

1. **Acceptance-command grammar was prefix-only (P1, security + adversarial + adversarial-codex + testing).** `COMMAND_GRAMMAR` matched only the `cargo <sub>` prefix, so `cargo test; rm -rf ~`, `cargo test && curl … | sh`, `$(…)`, backticks passed lint and reached `eval "$cmd"` in `verify-task.sh`. Fix: `command_is_safe()` rejects shell metacharacters; `--dump-acceptance` is now fail-closed (refuses unsafe commands, exit 2) so the eval sink is gated even when `verify-task.sh` runs standalone without the runner's lint preflight.
2. **Facade allowance licensed executable code (P1, correctness + adversarial, reproduced).** The per-line classifier inherited `line_in_stmt`, so `mod evil { fn … }`, code after a statement's `;`, `include!(…)`, and unclosed-`use`+code were all classified decl-only. Fix: `_scan_decl_line()` — statement-aware, character-restricted (`SAFE_DECL_CHARS`) scan on changed lines; brace-form module bodies and any non-`use`/`mod` residual fail. The legitimate A-382-2-r2 multi-line `pub use` case still licenses (replay pin green).
3. **Crashed agent reported success (P1, correctness + adversarial-codex).** `run-task.sh` captured `status=$?` but never branched on it; a crashed/unauthenticated `claude -p` produced an empty patch, cleared blast, and let the chain advance — the silent clean-base run the P1 amendment forbids. Fix: nonzero agent exit with no `UNKNOWN.md` is fatal (exit 1).
4. **`--stack` suppressed all missing deps (P1, correctness + adversarial-codex).** `have_stack = count > 0` let one unrelated patch cover every missing `depends_on`. Fix: count guard (`len(missing) > stack_count` refuses). Precise per-id acknowledgement deferred (see below).
5. **Lint ran against caller cwd, not the clone (P2, correctness).** Missing `--repo-root`, so a run from a `/tmp` scratch clone (the documented workflow) would FATAL on every contract. Fix: lint after rebuild with `--repo-root "$CLONE"`.
6. **Destructive `--clone` had no scratch guard (P1, adversarial-codex).** `ccx_rebuild_base` (`reset --hard` + `clean -fdq`) on the project root / a live checkout would erase work. Fix: `ccx_check_clone` refuses a clone that resolves to the harness's own repo and prints the resolved top-level.
7. **`run-tests.sh` swallowed shell-suite failures (P1, correctness).** A failing `test_runner.sh`/`test_verify.sh` still exited 0 — the vacuous-green hazard the gate exists to prevent. Fix: `|| exit 1` on both.
8. **R4 filter-branch deliverable downgrade (P2, correctness).** A bare vacuous filter (exits 0, vacuously green) was downgraded to a warning when allowed paths were in the same crate — the exact pilot Goodhart shape. Fix: bare-filter no-match is always an error (the `--test` deliverable exemption stays, since a missing `--test` target fails cargo loudly).
9. **Default-forbid was root-anchored (P2, security + correctness).** Nested `.env`/`.forge`/`.ssh`/`.aws` escaped the always-on deny list. Fix: added `**/`-prefixed twins.
10. **Unchecked `git add`/`mkdir`/`cat`, stale `UNKNOWN.md`, `UNKNOWN.md` in `patch.diff` (P1/P2, reliability + adversarial).** Fixed: guarded the commands; clear stale `UNKNOWN.md` at task start; keep it out of `patch.diff`.

Every fix carries a regression test (facade smuggling cases, injection/grammar, default-forbid nesting, R4 vacuous-inside-crate, crashed-agent, stack-count guard, injection-refused-before-eval). `verify-task.sh` tests now drive real cargo commands (the security gate forbids the old shell-string stand-ins).

## Deferred (filed in plan Open Questions)

- **`--stack` id matching.** Count guard closes the reproduced bypass; precise `--stack <id>=<patch>` / `--assume-dep <id>` is the follow-up.
- **`matches()` duplication** across `ccx-lint.py` / `ccx-blast.py` (maintainability P2). Left as two self-tested copies rather than adding sys.path-dependent sibling imports to standalone CLIs; extract if a third consumer appears.

## Defense-in-depth / residual (not blocking)

- Contract-id path traversal (`ccx-../../x`) is confined to `.yaml` files the operator holds and human-reviewed contracts; recorded as residual.
- Prompt injection via concatenated contract text into `--dangerously-skip-permissions` is the known/accepted harness-side surface (plan A4 / U5), deferred to the substrate phase.
- CI installs unpinned PyYAML at runtime — noted; the repo's 4-day min-release-age policy is npm/pnpm/bun/yarn, not pip, so out of scope here.

## Reviewed and rejected (known noise — skip re-flagging)

- `test_blast.py` fixture `SECRET=hunter2` / `.env` payloads are test inputs for the redaction/forbid logic, not leaked secrets (project-standards confirmed).
- `_scan_decl_line` lenient context-line tracking can mis-track on exotic facade context, but always biases a changed line toward **rejection** (safe direction) — not a finding.
