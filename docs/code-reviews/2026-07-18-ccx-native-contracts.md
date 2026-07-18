# Code review triage — CCX native contracts branch (U1–U10 + hardening)

- **Date:** 2026-07-18
- **Branch:** `brainstorm/ccx-harness-to-native` (local only)
- **base-sha:** `e8b1be28c765006fb1cceb228f0479d2e583aec6` (local main)
- **head-sha (reviewed):** `733e7be` (13 commits); review fixes land in the follow-up `fix(review)` commit `f2fb5ea`
- **Gate:** `/ce-code-review` — 11 personas (correctness, security, adversarial at session model; testing, maintainability, project-standards, api-contract, reliability, data-migration, agent-native, learnings at mid-tier) + Codex cross-model adversarial pass + 8-validator wave + orchestrator direct verification. Verify trio, `scripts/ci.sh` (95/95 e2e), and every per-unit review gate were green before this review. Plan `docs/plans/2026-07-10-001-feat-ccx-native-contracts-plan.md` passed as explicit requirements source.
- **Verdict:** Ready with fixes (all applied same-session in `f2fb5ea`; zero deferred findings; one pre-existing residual carries a pending ticket).

## Real-actionable (fixed in the `fix(review)` commit)

1. **Blast blind band** — `crates/forge-cli/src/commands/contract_blast.rs:442` required `is_ignored_by_policy && is_default_forbidden`, so agent-written files that are policy-excluded but not on DEFAULT_FORBID (`id_dsa`, `*.p12`, `*.pfx`, `*secret*`, singular `*credential*`) were dropped from snapshots and unflagged — a green run with the file silently gone. security (75) + adversarial (75), cross-agreed; validator CONFIRMED with the concrete set difference. Fixed: the scratch walk flags every policy-excluded file (fail-closed; materialization verified to write none itself), with a distinct `policy_excluded` violation detail.
2. **`--dep` acknowledgement unvalidated** — `commands/contract.rs:2048` overlaid any resolvable run's run-level tree with no contract-id/outcome/base checks; task-id refs ignored the task's own patch. correctness (75) + adversarial (75), cross-agreed; validator CONFIRMED. Fixed: `resolve_ack_dependency_ref` binds the ref to the acknowledged contract, requires completed outcome and matching `base_head`, resolves task refs to per-task patches (which also un-refuses the legitimate completed-task-in-stopped-run case); typed refusal on mismatch.
3. **Empty/typo'd acceptance verified green** — a nested-key typo (`acceptance: {fixes: [...]}`) was invisible to lint's top-level unknown-key strictness, R4 added no diagnostic on an empty union, and verify recorded a signed `passed=true` aggregate having executed zero commands. adversarial (75); validator CONFIRMED reachable through normal authoring. Fixed three-sided: lint errors on empty fix set and on unknown keys inside the acceptance mapping; verify refuses typed when zero commands parse (defense-in-depth, reachable only by tampered revisions post-lint-fix).
4. **Replay of run/verify exited 0 regardless of outcome** — replay envelopes carried only `idempotent_replay`, and `main.rs` defaulted the exit mapping to SUCCESS; the existing replay test coincidentally used a completed run. adversarial (75); validator CONFIRMED with full trace. Fixed: run/verify ops persist `exit_code`/`outcome` in view state; `replay_response` merges them; stop replays exit 2, guard-regressed verify replays exit 4 (tested).
5. **Doctor blind to row+signature co-deletion** — contract-family op digests are recovered from frozen op `state_json` (unlike evidence/decisions which recompute from live rows), so deleting a `contract_stops` row plus its signature rows left doctor green while un-blocking Leg-3 — a new, contract-family-specific asymmetry. adversarial (75); validator CONFIRMED as newly introduced. Fixed: `contract_referenced_row_issues` doctor pass cross-checks every contract-family op's referenced rows exist; co-deletion of stop and run rows now reported (tested).
6. **Verify verdicts not revision-bound** — `--revision` selected which acceptance set ran, but the chosen revision appeared only in unsigned CLI output, never in the signed verdict content. adversarial (75); validator CONFIRMED. Fixed: `revision` column added to verdict rows and folded into the signed digest; surfaced in the verdicts query.
7. **Mid-chain IO/store error left the run unrecorded** — fallible per-task steps `?`-propagated without recording any run row (validator narrowed: a bad agent command lands in the recorded exit-127 path; the window is IO/store/diff failures). reliability (75). Fixed: fallible steps wrapped to record a failed run (redacted error, dependents skipped) before propagating.
8. **Duplicate YAML id-list parsers** — `parse_depends_on` (CLI) and `parse_brief_neighbors` (store) reimplemented identical logic. maintainability (100); orchestrator direct-verified. Fixed: shared `parse_yaml_id_list_field`.
9. **Failure-recording prelude duplicated ×4** — stop/crash/empty/blast branches repeated the push+`fill_skipped`+build sequence (a forget-`fill_skipped` trap). maintainability (75); validator CONFIRMED non-cosmetic. Fixed: `build_failed_run_input` helper, also used by fix 7's new path.
10. **CE items** — `CONTRACT_NOT_INTEGRABLE` added to both `forge_schema.rs` drift-guard lists (learnings + agent-native, independently); never-constructed-variant comments added to `ContractStopMalformed`/`ContractBlastViolation` (api-contract residual); **`forge contract runs [--contract-id] [--outcome]`** read surface added — a failed/blast-violated run's id was otherwise unrecoverable after the invoking stdout was gone, breaking the plan's cold-start-agent-operator criterion (agent-native Warning).

## Defer-able (tickets)

- **gc pack-repack residual** (pre-review, carried): `gc` repacks unreachable objects without reachability filtering and the fresh pack's `packed_at_ms` resets the protection window, so secret-content-refused post-trees linger zstd-compressed in `.forge/packs` until the window elapses. Pinned by the flip-guard in `gc_reclaims_secret_refused_unreferenced_post_tree`. Linear ticket drafted; filing blocked on expired `linear-server` auth — re-file on re-auth. Candidate fixes: reachability-filter the repack, or inherit loose mtime as `packed_at_ms`.

## Defense-in-depth / residual (recorded, no action)

- Acceptance commands and `--agent-cmd` run unsandboxed with operator privileges; `cargo test/build/run` executes agent-authored build.rs/test code — inherent documented harness posture, the feature's largest trust surface (security).
- The cargo grammar gate blocks metacharacters, not argument-level steering (`--config build.rustc-wrapper=...`); parity with the harness, but the gate must not be described as preventing arbitrary execution (security).
- Store-level `record_contract_run` trusts callers to pre-redact agent excerpts (CLI does); a future library caller could sign unredacted output (security).
- Doctor's op-chain check recovers contract digests from frozen op state — the signature recompute pass is load-bearing and must not be weakened independently (security; partially mitigated by fix 5's row cross-check).
- `contract_integration_accepted` is temporal-blind: an accept-ever-existed satisfies the deps gate even after undo/checkout moved HEAD (adversarial residual).
- `VERIFY_COMMAND_TIMEOUT_MS` (180s) assumes small trees; a cold build of a large workspace could mis-record `fix_failed` (adversarial residual).
- Symlink handling through snapshot→blast unpinned by tests; ported `glob_match` lets `*` cross `/` (fnmatch parity — broader than Rust-glob intuition).
- Repo advisory lock held across the agent subprocess with no timeout (deliberate single-run-at-a-time design, documented in code).
- `redacted_excerpt` single `read` call could short-read on exotic file types (theoretical for regular files).
- FK relationships (`resolving_revision`, task linkage) enforced in application code, not schema; no delete path exists yet (data-migration residual).
- `forge-store/src/error.rs` hand-rolled enum vs the CLAUDE.md "anyhow-only" phrasing — pre-existing convention drift worth reconciling in docs (project-standards).
- Facade decl-only files skip the secret-content scan; safe-decl chars block realistic literals (correctness residual).
- `parse_unknown_fields` matches stop `kind` case-sensitively — fail-closed (marks malformed) but forces avoidable triage (correctness residual).

## Reviewed-and-rejected / known noise (do not re-flag)

- The gc pack-repack residual, serde_yaml 0.9.34 frozen-crate decision, 1MiB/non-UTF8 secret-scan bounds, and all per-unit-review fixes were pre-triaged inputs to this review; no reviewer re-raised them as findings.
- Plan frontmatter carries no `status:` field — deliberate per the ce-unified-plan/v1 artifact contract, not drift.
- `forge contract list` for contract-id discovery deliberately absent: authoring is file-based (`ls contracts/*.yaml`), shared-workspace by design.

## Coverage notes

- Codex cross-model pass: **zero findings, zero residuals** (second fully clean cross-model pass on this surface).
- Validators: 8 dispatched, 8 confirmed (1 narrowed: mid-chain window excludes command-not-found), 0 infra failures; 1 anchor-100 mechanical finding verified by orchestrator quote-check instead of a validator.
- Cross-reviewer agreement promoted 2 findings to anchor 100 (blast blind-band: security+adversarial; `--dep` ack: correctness+adversarial).
- Zero findings from: testing, api-contract, data-migration, project-standards, fast-pass (their signal routed to testing-gaps/residuals above). Preliminary fast-pass items withdrawn: 0 (none were raised).
- Testing gaps carried (not blocking): per-kind tamper-injection for run/verdict subjects; Python-parity fixture for the decl-only scanner; REQUEST_ID_CONFLICT (different-args) for contract commands; capitalized stop-kind parsing pin; symlink pipeline pin; near-timeout verify.
- Requirements completeness (explicit plan): R1–R27 and U1–U10 all implemented and traced (AE1–AE11 each carry a tagged test; the R21 retirement-criterion dogfood passes). No unaddressed requirements.
- Repo-profile cache skipped (MISS at reviewed head; optimization only). Untracked `docs/.DS_Store` excluded from scope.
