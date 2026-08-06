# Dogfood #3 record — gc repack reachability via the native contract surface

- **Date:** 2026-07-18
- **Contract:** `contracts/gc-repack-reachability.yaml` (`ccx-gc-repack-reachability`, revision 1) + `contracts/_global-policy.yaml`, both frozen through `forge contract freeze`
- **Surface under test:** the full native `forge contract` family — this is the R21 retirement-criterion run named in `docs/plans/completed/2026-07-10-001-feat-ccx-native-contracts-plan.md`
- **Implementation agent:** real `claude -p --model claude-fable-5` (the only script glue), operating in the forge-materialized scratch workspace of a `/tmp` clone
- **Operator:** session agent driving the binary directly; triage decisions by the session/product authority

## Run 1 — blast violation (exit 3): a true tooling finding

The agent implemented the fix, but the run was refused: `secret-content detected in crates/forge-cli/tests/forge_contract.rs`. Root cause: the U6 secret-content scan evaluated the whole post-state of modified files, re-flagging pre-existing secret-shaped fixture strings already signed into the base tree — content the agent did not author, in the very file the contract required editing (the flip-guard test).

Notable machinery wins even in refusal:
- The violated run was discoverable cold via `forge contract runs --json` — the query surface added by the branch review (F12) for exactly this scenario.
- The gate failed closed: verdict recorded, patch not persisted, dependents halted, exit 3 with envelope `outcome: blast_violation`.

**Triage:** tooling defect, not contract ambiguity — no revision bump. Fixed as diff-aware scanning (`feb02a9`): modified files scan only agent-added lines against the baseline blob (new `forge-content-native` blob-read module); added files still scan whole; new-secret refusal, bounds, and path-only reporting unchanged. Regression test reproduces the exact run-1 scenario.

## Run 2 — completed (exit 0), chain green end-to-end

`contract_run_019f758407377681b8b598ca5570c9dc`: run completed → `verify` green (blast pass; fix: `cargo test -p forge-cli --test forge_contract`, `cargo test -p forge-store`; guards: pack-gc tests, content-native tests, workspace clippy; aggregate `passed`) → `integrate` produced attempt + synthesized intent → `save → run → propose → check → accept` → **doctor green** (0 signature issues, 0 tampered rows, 0 contract-row issues).

## The produced fix (landed as `b5d7f3c` after review)

Reachability-filtered repack: unreachable, unprotected loose objects are never pack candidates; they are deleted outright via a new `deletable_loose_native_objects` plan bucket, folded into the plan digest (version bumped v2→v3 so stale dry-run digests cannot authorize the new deletions), with a crash-safety hook per deletion. The flip-guard test now asserts the refused secret is gone from the loose store AND every pack (decompressed read-back) in a single gc cycle; `forge_pack_gc` assertions strengthened, not weakened. Review tail (independent, post-verify): verdict **SHIP**, three non-blocking nits (distinct crash-point name for the new loop; restore exact-bytes post-crash assertion; collapse a redundant dry-run call).

## Verdict inputs for the release audit

- R21 satisfied in anger; per the staged-stability decision the contract ledger kinds + envelope surfaces are adopter-grade from this run; `tools/ccx` script stages retired (frozen reference).
- Stop-gate legs were not exercised by a filed UNKNOWN this run (run 1's refusal was a blast verdict, not a stop); the legs are covered by the retirement integration test and dogfoods #1–#2. First real-world UNKNOWN triage through `contract resolve` remains a watch item for dogfood #4.
- Follow-up nits (non-blocking): crash-point isolation test for the orphan-deletion loop; exact-bytes post-crash assertion; redundant dry-run call in the flip-guard test.
